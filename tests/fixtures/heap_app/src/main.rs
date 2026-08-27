fn main() {
    // Several distinct heap allocations for the tracker to observe
    let _a: Vec<u8> = vec![0u8; 256];
    let _b: String = String::from("hello, baxan");
    let _c: Box<[u64]> = vec![1, 2, 3, 4, 5].into_boxed_slice();
    let _d: Vec<String> = (0..10).map(|i| format!("item-{i}")).collect();
    // Force a drop before exit so the tracker sees deallocations
    drop(_a);
    drop(_d);
}
