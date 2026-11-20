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
        ("benchmark 0 512K 1 --mem-node 1023", "numa node 1023 does not exist"),
        ("benchmark 0 512K 1 --huge 4K", "invalid --huge"),
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

// without root, ftrace is out of reach: say so before measuring anything
#[test]
fn ftrace_needs_access() {
    // SAFETY: geteuid takes no arguments
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for args in ["benchmark 0 512K 1 --tracemark", "top 0 512K --breaktrace 100"] {
        let o = memlat(args);
        assert_eq!(o.status.code(), Some(1), "{args}");
        assert!(stderr(&o).contains("ftrace:"), "{args}: {}", stderr(&o));
    }
}

// --json - leaves stdout to the JSON alone (c2c: quick, no settle phase)
#[test]
fn json_on_stdout() {
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return;
    }
    let o = memlat("c2c 0,1 100 --json -");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with("{\n") && out.ends_with("}\n"), "{out}");
    assert!(out.contains("\"mode\": \"c2c\""), "{out}");
    assert!(out.contains("\"median_ns\": "), "{out}");
    assert!(!out.contains("closest  pair"), "text report mixed in: {out}");
}

// the number after `"key": ` in a JSON text
fn json_num(s: &str, key: &str) -> f64 {
    let pat = format!("\"{key}\": ");
    let i = s.find(&pat).unwrap_or_else(|| panic!("{key} missing")) + pat.len();
    let end = s[i..].find([',', '\n']).map_or(s.len(), |e| i + e);
    s[i..end].parse().unwrap_or_else(|_| panic!("{key}: {}", &s[i..end]))
}

// a real (short) measurement from start to report: 2 s settle + 1 s
#[test]
fn benchmark_end_to_end() {
    let o = memlat("benchmark 0 1M 1 --huge none --json -");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("\"mode\": \"benchmark\""), "{out}");
    assert!(out.contains("\"pages\": \"4K pages, THP off\""), "{out}");
    assert!(json_num(&out, "iterations") > 1e5, "{out}");
    let d = json_num(&out, "duration_s");
    assert!((0.9..2.0).contains(&d), "duration {d}");
    assert!(json_num(&out, "lines") == (1 << 20) as f64 / 64.0);
    assert!(json_num(&out, "max_us") > 0.0);
    // the text report stays off stdout
    assert!(!out.contains("time per iteration histogram"), "{out}");
}
