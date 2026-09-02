use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "gen-icon" => gen_icon(),
        other => bail!("Bilinmeyen komut: '{other}'. Kullanım: cargo run -p xtask -- gen-icon"),
    }
}

fn gen_icon() -> Result<()> {
    let img = image::open("icon.png")
        .context("icon.png okunamadı — bu komutu repo KÖKÜNDEN çalıştırdığından emin ol")?;
    let img = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
    std::fs::create_dir_all("assets").context("assets/ dizini oluşturulamadı")?;
    img.save_with_format("assets/moku.ico", image::ImageFormat::Ico)
        .context("ICO formatına kaydedilemedi")?;
    println!("assets/moku.ico oluşturuldu. Şimdi bunu git'e ekle: git add assets/moku.ico");
    Ok(())
}
