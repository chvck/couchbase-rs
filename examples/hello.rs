use std::error::Error;
use couchbase::Cluster;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let cluster = Cluster::connect("127.0.0.1");
    let bucket = cluster.bucket("travel-sample");
    let collection = bucket.default_collection();

    println!("{:?}", collection.get("foo", None).await);

    Ok(())
}