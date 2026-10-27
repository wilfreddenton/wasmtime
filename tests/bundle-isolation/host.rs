use std::thread;

use bundle_first::run as first;
use bundle_second::run as second;

fn main() {
    assert_eq!(first().0, 4);
    assert_eq!(second().0, 16);
    // The second VM installs a handler after the first. Exercise the first again
    // to catch a handler chain that accidentally captures only the newest VM.
    assert_eq!(first().0, 4);
    for _ in 0..4 {
        let left = thread::spawn(first);
        let right = thread::spawn(second);
        assert_eq!(left.join().unwrap().0, 4);
        assert_eq!(right.join().unwrap().0, 16);
    }
    println!(
        "dual VM: distinct lifting charges, memory/stack traps, async fuel yields, concurrent replay passed"
    );
}
