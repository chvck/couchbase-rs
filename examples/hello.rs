use couchbase::Cluster;
use std::error::Error;
use std::sync::Arc;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let cluster = Arc::new(Cluster::connect("127.0.0.1", "Administrator", "password"));
    let bucket = cluster.bucket("travel-sample");
    let _collection = bucket.default_collection();

    println!("{:?}", cluster.query("select 1=1", None).await.unwrap());

    Ok(())
}
