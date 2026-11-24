pub fn run(argv: &[String]) -> i32 {
    use clap::Parser;
    let argv: Vec<String> = argv.to_vec();
    // Bare invocation is a sidecar: that is the argv `gray plugin check`
    // and a profile `sidecar:` line both spawn, and it keeps the daemon
    // contract identical whether or not the caller names the mode.
    if argv.len() == 1 {
        let path = crate::channels::telegram::config::default_path();
        let path = std::path::absolute(&path).unwrap_or(path);
        crate::channels::telegram::sidecar::serve(&path);
    }
    let cli = crate::channels::telegram::cli::Cli::parse_from(argv);
    // Absolute, so the service, the lock argv and the saved workdir agree
    // no matter where setup was run from.
    let path = cli.config_path();
    let path = std::path::absolute(&path).unwrap_or(path);
    match crate::channels::telegram::cli::run(&cli.command, &path) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    }
    0
}
