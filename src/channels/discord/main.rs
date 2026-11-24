pub fn run(argv: &[String]) -> i32 {
    use clap::Parser;
    let cli = crate::channels::discord::cli::Cli::parse_from(argv);
    // Absolute, so the service, the lock argv and the saved workdir agree
    // no matter where setup was run from.
    let path = cli.config_path();
    let path = std::path::absolute(&path).unwrap_or(path);
    match crate::channels::discord::cli::run(&cli.command, &path) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    }
    0
}
