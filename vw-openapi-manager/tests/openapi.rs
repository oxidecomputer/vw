// The OpenAPI documents under `openapi/` are what `vw-api-client` generates
// its progenitor clients from. Nothing forces them to be regenerated when an
// endpoint changes, so an out of date document would quietly leave the client
// describing an API the service no longer serves. Catch that here rather than
// at runtime.

use dropshot_api_manager::test_util::{check_apis_up_to_date, CheckResult};

#[test]
fn openapi_documents_are_up_to_date() {
    let environment =
        vw_openapi_manager::environment().expect("resolve environment");
    let apis = vw_openapi_manager::all_apis().expect("collect managed apis");

    match check_apis_up_to_date(&environment, &apis).expect("run check") {
        CheckResult::Success => {}
        CheckResult::NeedsUpdate => {
            panic!("openapi documents are out of date; run `cargo xtask openapi generate`")
        }
        CheckResult::Failures => {
            panic!("openapi documents failed validation; see the output above")
        }
    }
}
