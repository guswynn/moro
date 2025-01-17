#[tokio::main]
async fn main() {
    eprintln!("all positive {:?}", run(vec![1, 2, 10]).await);
    eprintln!("some negative {:?}", run(vec![1, 2, -3, 10]).await);
}

/// Run the simulated journal.
pub async fn run(inputs: Vec<i32>) -> anyhow::Result<()> {
    moro::async_scope!(|scope| {
        for input in &inputs {
            let _ = scope.spawn_cancelling(validate(input));
        }
    })
    .await
}

pub async fn validate(input: &i32) -> anyhow::Result<()> {
    if *input < 0 {
        anyhow::bail!("input out of range: {input}");
    }
    Ok(())
}
