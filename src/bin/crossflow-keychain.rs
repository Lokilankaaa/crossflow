//! Dedicated one-request native Keychain process. Never captures OS input.
fn main() {
    if std::env::args_os().len() != 1 {
        std::process::exit(2);
    }
    let result = crossflow::keychain_process::run_worker();
    // Do not print protocol bodies, locators, secrets or framework error chains.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
