use std::sync::{Arc, Barrier};
use std::thread;

use bundle_first::run as first;
use bundle_second::run as second;

fn main() {
    let expected = if env!("BUNDLE_TEST_CONFIGURATION") == "kontor" {
        (1026, 1028)
    } else {
        (4, 16)
    };
    assert_eq!(first().0, expected.0);
    assert_eq!(second().0, expected.1);
    // The second VM installs a handler after the first. Exercise the first again
    // to catch a handler chain that accidentally captures only the newest VM.
    assert_eq!(first().0, expected.0);
    let barrier = Arc::new(Barrier::new(2));
    let left_barrier = barrier.clone();
    let left = thread::spawn(move || {
        left_barrier.wait();
        for _ in 0..100 {
            assert_eq!(first().0, expected.0);
            assert_eq!(second().0, expected.1);
        }
    });
    let right = thread::spawn(move || {
        barrier.wait();
        for _ in 0..100 {
            assert_eq!(second().0, expected.1);
            assert_eq!(first().0, expected.0);
        }
    });
    left.join().unwrap();
    right.join().unwrap();
    println!(
        "dual VM: distinct lifting charges, memory/stack traps, async fuel yields, concurrent replay passed"
    );
}
