pub fn run(argv: &[String]) -> i32 {
    use clap::Parser;
    // `gray plugin check` and the plugin host spawn the binary bare; with no
    // subcommand on a pipe, this process IS the sidecar. On a terminal a
    // bare run prints help instead of sitting silent.
    let args: Vec<String> = argv.to_vec();
    let subcommand_given = args.iter().skip(1).any(|a| !a.starts_with('-'));
    // -h/--help/-V/--version go to clap; without this they fell into sidecar
    // mode whenever stdin wasn't a terminal and printed nothing (exit 0).
    let help_flag = args
        .iter()
        .skip(1)
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version"));
    if !subcommand_given && !help_flag {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            let config = arg_after(&args, "--config")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(crate::channels::slack::config::default_path);
            let config = std::path::absolute(&config).unwrap_or(config);
            crate::channels::slack::sidecar::serve(&config);
        }
        let mut cmd = <crate::channels::slack::cli::Cli as clap::CommandFactory>::command();
        let _ = cmd.print_help();
        println!();
        std::process::exit(2);
    }
    let cli = crate::channels::slack::cli::Cli::parse_from(argv);
    // Absolute, so the service, the lock argv and the saved workdir agree
    // no matter where setup was run from.
    let path = cli.config_path();
    let path = std::path::absolute(&path).unwrap_or(path);
    match crate::channels::slack::cli::run(&cli.command, &path) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    }
    0
}

fn arg_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix(&format!("{flag}=")))
                .map(str::to_string)
        })
}
