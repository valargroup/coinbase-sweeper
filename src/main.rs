fn main() {
    let code = coinbase_sweeper::cli::main(std::env::args_os());
    std::process::exit(code as i32);
}
