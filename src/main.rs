use anyhow::Result;
use reqwest::Client;

#[path = "Arena/mod.rs"]
pub mod arena;
#[path = "Feladatok/mod.rs"]
pub mod feladatok;
#[path = "Kuldetesek/mod.rs"]
pub mod kuldetesek;

#[tokio::main]
async fn main() -> Result<()> {
    let CLIENT = Client::new();

    //? A1
    // arena::a1::solve(&CLIENT).await?;
    //? 1
    // feladatok::fejlec::solve(&CLIENT).await?;
    //? 2
    // feladatok::archivum::solve(&CLIENT).await?;
    //? 7
    // feladatok::ballisztika::solve(&CLIENT).await?;
    //? 8
    // feladatok::cimke::solve(&CLIENT).await?;
    // ? 9
    // feladatok::handshake::solve(&CLIENT).await?;

    //* Mellék küldetések:
    //? S2
    // kuldetesek::s2::solve(&CLIENT).await?;

    //* Aréna:
    //? A1
    arena::a1::solve(&CLIENT).await?;
    //? A2
    // arena::a2::solve(&CLIENT).await?;

    Ok(())
}
