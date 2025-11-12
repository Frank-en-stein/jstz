use std::{collections::HashSet, path::PathBuf};

use anyhow::anyhow;
use clap::Args;
use deno_core::{ModuleSpecifier, StaticModuleLoader};
use indexmap::IndexMap;
use jstz_crypto::{hash::Hash as _, smart_function_hash::SmartFunctionHash};
use jstz_proto::runtime::v2::{fetch::ProtoFetchHandler, ledger::jstz_ledger};
use jstz_runtime::{
    jstz_test::{
        create_test_event_channel, executor::test_specifier, jstz_test, TestEvent,
        TestEventReceiver, TestFailureFormatOptions, TestResult,
    },
    runtime::MAX_SMART_FUNCTION_CALL_COUNT,
};
use tokio::time::Instant;
use url::Url;

use crate::test::reporter::{PrettyTestReporter, TestReporter};

mod reporter;

#[derive(Debug, Clone, Args)]
pub struct TestArgs {
    /// List of file names to run
    #[arg(value_name = "FILES", value_hint = clap::ValueHint::FilePath)]
    files: Vec<PathBuf>,
}

/// Gives receiver back in case it was ended with `TestEvent::ForceEndReport`.
pub async fn report_tests(
    mut receiver: TestEventReceiver,
    mut reporter: Box<dyn TestReporter>,
) -> (anyhow::Result<()>, TestEventReceiver) {
    let mut tests = IndexMap::new();
    let mut test_steps = IndexMap::new();
    let mut tests_started = HashSet::new();
    let mut tests_with_result = HashSet::new();
    let mut start_time = None;
    let mut had_plan = false;
    let mut used_only = false;
    let mut failed = false;

    while let Some((_, event)) = receiver.recv().await {
        match event {
            TestEvent::Register(description) => {
                for (_, description) in description.into_iter() {
                    reporter.report_register(description);
                    // TODO(mmastrac): We shouldn't need to clone here -- we can reuse the descriptions everywhere
                    tests.insert(description.id, description.clone());
                }
            }
            TestEvent::Plan(plan) => {
                if !had_plan {
                    start_time = Some(Instant::now());
                    had_plan = true;
                }
                if plan.used_only {
                    used_only = true;
                }
                reporter.report_plan(&plan);
            }
            TestEvent::Wait(id) => {
                if tests_started.insert(id) {
                    reporter.report_wait(tests.get(&id).unwrap());
                }
            }
            TestEvent::Output(output) => {
                reporter.report_output(&output);
            }
            TestEvent::Slow(id, elapsed) => {
                reporter.report_slow(tests.get(&id).unwrap(), elapsed);
            }
            TestEvent::Result(id, result, elapsed) => {
                if tests_with_result.insert(id) {
                    match result {
                        TestResult::Failed(_) | TestResult::Cancelled => {
                            failed = true;
                        }
                        _ => (),
                    }
                    reporter.report_result(tests.get(&id).unwrap(), &result, elapsed);
                }
            }
            TestEvent::UncaughtError(origin, error) => {
                failed = true;
                reporter.report_uncaught_error(&origin, error);
            }
            TestEvent::StepRegister(description) => {
                reporter.report_step_register(&description);
                test_steps.insert(description.id, description);
            }
            TestEvent::StepWait(id) => {
                if tests_started.insert(id) {
                    reporter.report_step_wait(test_steps.get(&id).unwrap());
                }
            }
            TestEvent::StepResult(id, result, duration) => {
                if tests_with_result.insert(id) {
                    reporter.report_step_result(
                        test_steps.get(&id).unwrap(),
                        &result,
                        duration,
                        &tests,
                        &test_steps,
                    );
                }
            }
            TestEvent::ForceEndReport => {
                break;
            }
            TestEvent::Completed => {
                reporter.report_completed();
            }
            TestEvent::Sigint => {
                let elapsed = start_time
                    .map(|t| Instant::now().duration_since(t))
                    .unwrap_or_default();
                reporter.report_sigint(
                    &tests_started
                        .difference(&tests_with_result)
                        .copied()
                        .collect(),
                    &tests,
                    &test_steps,
                );

                #[allow(clippy::print_stderr)]
                if let Err(err) = reporter.flush_report(&elapsed, &tests, &test_steps) {
                    eprint!("Test reporter failed to flush: {}", err)
                }
                #[allow(clippy::disallowed_methods)]
                std::process::exit(130);
            }
        }
    }

    let elapsed = start_time
        .map(|t| Instant::now().duration_since(t))
        .unwrap_or_default();
    reporter.report_summary(&elapsed, &tests, &test_steps);
    if let Err(err) = reporter.flush_report(&elapsed, &tests, &test_steps) {
        return (
            Err(anyhow!("Test reporter failed to flush: {}", err)),
            receiver,
        );
    }

    if used_only {
        return (
            Err(anyhow!("Test failed because the \"only\" option was used",)),
            receiver,
        );
    }

    if failed {
        return (Err(anyhow!("Test failed")), receiver);
    }

    (Ok(()), receiver)
}

const TEST_RUNNER_ADDRESS: &str = "KT1RJ6PbjHpwc3M5rw5s2Nbmefwbuwbdxton";
const TEST_RUNNER_MAX_SMART_FUNCTION_CALL_COUNT: u8 = MAX_SMART_FUNCTION_CALL_COUNT;

pub(crate) async fn exec(args: TestArgs) -> anyhow::Result<()> {
    for file in args.files {
        let fname = file.display();
        let code = std::fs::read_to_string(&file)
            .map_err(|e| anyhow!("Failed to read test file {}: {}", fname, e))?;

        // Initialize a mocked Jstz runtime
        let mut init_host = tezos_smart_rollup_mock::MockHost::default();
        let mut init_tx = jstz_core::kv::Transaction::default();
        init_tx.begin();

        let specifier = ModuleSpecifier::from_file_path(file.canonicalize()?)
            .map_err(|_| anyhow!("Invalid file path '{}'", fname))?;

        let module_loader = StaticModuleLoader::with(specifier.clone(), code);

        let init_addr = SmartFunctionHash::from_base58(TEST_RUNNER_ADDRESS).unwrap();
        let request_id = format!("test-runner-{}", fname);

        let protocol =
          jstz_runtime::RuntimeContext::new(
              &mut init_host,
              &mut init_tx,
              init_addr.clone(),
              request_id,
              jstz_runtime::runtime::Limiter::<
                  TEST_RUNNER_MAX_SMART_FUNCTION_CALL_COUNT,
              >::default()
              .try_acquire()
              .expect("Failred to acquire limiter for test runner"),
          );

        let (sender, reciever) = create_test_event_channel();

        let mut runtime =
            jstz_runtime::JstzRuntime::new(jstz_runtime::JstzRuntimeOptions {
                module_loader: std::rc::Rc::new(module_loader),
                fetch: ProtoFetchHandler,
                protocol: Some(protocol),
                extensions: vec![
                    jstz_ledger::init_ops_and_esm(),
                    jstz_test::init_ops_and_esm(sender),
                ],
                snapshot: None,
            });

        test_specifier(&mut runtime, specifier).await?;

        drop(runtime);

        let pretty_reporter = PrettyTestReporter::new(
            false,
            false,
            false,
            false,
            Url::from_directory_path(std::env::current_dir().expect("Failed to get cwd"))
                .map_err(|_| {
                    anyhow!(
                        "Failed to convert current directory to URL for test reporter"
                    )
                })?,
            TestFailureFormatOptions::default(),
        );

        let (result, _) = report_tests(reciever, Box::new(pretty_reporter)).await;

        result?;
    }

    return Ok(());
}
