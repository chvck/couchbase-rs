use std::error::Error;
use couchbase::Cluster;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let cluster = Cluster::connect("127.0.0.1");
    let bucket = cluster.bucket("travel-sample");
    let _collection = bucket.default_collection();

    println!("{:?}", cluster.query("select 1=1", None).await);

    Ok(())
}