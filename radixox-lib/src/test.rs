use crate::shared_byte::SharedByte;

#[test]
fn test_str() {
    let byte = SharedByte::from_slice(b"Salut !");
    let cpy = byte.clone();
    println!("The cpy value is: {}\n", cpy.as_str().expect("should work"))
}
#[test]
fn verify_niche() {
    assert_eq!(
        std::mem::size_of::<SharedByte>(),
        std::mem::size_of::<Option<SharedByte>>()
    );
}
