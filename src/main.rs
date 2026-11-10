fn main() {
    if let Err(error) = raja_core::app::run_cli(std::env::args_os().skip(1)) {
        eprintln!("Raja: {error}");
        std::process::exit(1);
    }
}
