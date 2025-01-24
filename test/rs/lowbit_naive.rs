//# Compute the lowest 1 bit of an integer in the naive way
//# ARGS: 21324
fn main(n:i64) {
    let lb : i64 = 1;
    while (n == n / 2 * 2) {
        n = n / 2;
        lb = lb * 2;
    }
    println!("{:?}", lb);
}
