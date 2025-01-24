//# Testing Bitand and (integer) Neg
//# ARGS: 7 19
fn main(n:i64, m:i64) {
    let res : i64 = n & m;
    println!("{:?}", res);
    res = -res;
    println!("{:?}", res);
}
