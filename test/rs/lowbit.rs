//# Compute the lowest 1 bit of an integer
//# ARGS: 21324
fn main(n:i64) {
    let lb : i64 = n & (-n);
    println!("{:?}", lb);
}
