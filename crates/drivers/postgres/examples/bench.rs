//! Phase 0 throughput probe for the PostgreSQL -> Arrow path.
//!
//! Reports the two numbers the exit criteria are written against: time to first
//! batch (what the user perceives as "the grid appeared") and total time to
//! drain the result (what determines whether scrolling can stay ahead of the
//! user). Peak RSS is measured externally via `/usr/bin/time -l`.
//!
//! Usage: cargo run --release --example bench -- [batch_rows]

use driver_postgres::PgSource;
use std::time::Instant;

const CONN: &str = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench";
const SQL: &str = "SELECT * FROM bench_wide";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let batch_rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192);
    // A grid scrolls backwards, so the real memory question is what it costs to
    // hold the whole result, not to stream past it.
    let retain = std::env::args().any(|a| a == "--retain");

    let t0 = Instant::now();
    let src = PgSource::connect(CONN).await?;
    let connect_ms = t0.elapsed().as_secs_f64() * 1e3;

    let t1 = Instant::now();
    let mut stream = src.query(SQL, batch_rows).await?;
    let prepare_ms = t1.elapsed().as_secs_f64() * 1e3;

    let schema = stream.schema();

    let t2 = Instant::now();
    let mut first_batch_ms = f64::NAN;
    let mut rows = 0usize;
    let mut batches = 0usize;
    let mut bytes = 0usize;
    let mut held = Vec::new();

    while let Some(batch) = stream.next_batch().await? {
        if batches == 0 {
            first_batch_ms = t2.elapsed().as_secs_f64() * 1e3;
        }
        rows += batch.num_rows();
        bytes += batch.get_array_memory_size();
        batches += 1;
        if retain {
            held.push(batch);
        }
    }
    let total_s = t2.elapsed().as_secs_f64();
    println!("retained         {}", held.len());

    println!("columns          {}", schema.fields().len());
    println!("batch_rows       {batch_rows}");
    println!("connect_ms       {connect_ms:.1}");
    println!("prepare_ms       {prepare_ms:.1}");
    println!("first_batch_ms   {first_batch_ms:.1}");
    println!("rows             {rows}");
    println!("batches          {batches}");
    println!(
        "arrow_bytes      {bytes} ({:.1} MiB)",
        bytes as f64 / 1048576.0
    );
    println!("total_s          {total_s:.3}");
    println!("rows_per_s       {:.0}", rows as f64 / total_s);
    Ok(())
}
