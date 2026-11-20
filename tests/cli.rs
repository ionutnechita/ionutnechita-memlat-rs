// The binary's exit codes and messages for arguments that never start a
// measurement.

use std::process::{Command, Output};

fn memlat(args: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_memlat")).args(args.split_whitespace()).output().expect("run memlat")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_goes_to_stdout() {
    for args in ["--help", "-h", "help", "benchmark 2 512K 60 --help"] {
        let o = memlat(args);
        assert_eq!(o.status.code(), Some(0), "{args}");
        assert!(stdout(&o).starts_with("usage:"), "{args}");
        assert!(stderr(&o).is_empty(), "{args}");
    }
}

#[test]
fn version() {
    for args in ["--version", "-V", "version"] {
        let o = memlat(args);
        assert_eq!(o.status.code(), Some(0), "{args}");
        assert_eq!(stdout(&o).trim(), format!("memlat {}", env!("CARGO_PKG_VERSION")), "{args}");
    }
}

#[test]
fn usage_goes_to_stderr() {
    for args in ["", "bogus", "benchmark", "benchmark 2 512K", "top 2", "c2c"] {
        let o = memlat(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(stdout(&o).is_empty(), "{args:?}");
        assert!(stderr(&o).starts_with("usage:"), "{args:?}");
    }
}

#[test]
fn invalid_values_say_why() {
    let cases = [
        ("benchmark 5000 512K 1", "cpu 5000 does not exist"),
        ("benchmark 1023 512K 1", "cpu 1023 does not exist or is offline"),
        ("5000 512K 1", "cpu 5000 does not exist"),
        ("benchmark 0 17179869185G 1", "invalid size"),
        ("benchmark 0 64 1", "size too small"),
        ("benchmark 0 512K 0", "invalid seconds"),
        ("benchmark 0 512K 1 --fifo 99", "fifo priority must be 1-98"),
        ("benchmark 0 512K 1 --bogus 1", "unknown option: --bogus"),
        ("benchmark 0 512K 1 --log", "--log needs a value"),
        ("benchmark 0 1 --pingpong 0", "different partner cpu"),
        ("top 0,1 16M", "top measures one cpu"),
        // far more than any machine running the tests has
        ("benchmark 0 1000000G 1", "is available"),
        ("c2c 0", "c2c needs at least two cpus"),
    ];
    for (args, msg) in cases {
        let o = memlat(args);
        assert_eq!(o.status.code(), Some(1), "{args}");
        assert!(stderr(&o).contains(msg), "{args}: {}", stderr(&o));
    }
}
