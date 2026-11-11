#[tarpc::service]
pub trait Core {
    async fn add(a: i32, b: i32) -> i32;
}
