use std::time::Instant;
fn med(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
#[ignore]
fn split() {
    const STACK: usize = 64 * 1024 * 1024;
    let mut spawn = vec![];
    for _ in 0..200 {
        let t = Instant::now();
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(STACK)
                .spawn_scoped(s, || std::hint::black_box(1u32))
                .unwrap()
                .join()
                .unwrap();
        });
        spawn.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let mut small = vec![];
    for _ in 0..200 {
        let t = Instant::now();
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn_scoped(s, || std::hint::black_box(1u32))
                .unwrap()
                .join()
                .unwrap();
        });
        small.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!("\n  thread spawn+join, doing nothing:");
    println!("    64MB stack   {:>7.3} ms", med(spawn));
    println!("     2MB stack   {:>7.3} ms", med(small));
}
