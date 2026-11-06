fn main() {
    #[cfg(windows)]
    if let Err(error) = mwb::input_service::run() {
        eprintln!("MWB input service: {error:#}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("The input service requires Windows.");
        std::process::exit(1);
    }
}
