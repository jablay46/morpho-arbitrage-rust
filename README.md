# Morpho Flashloan Arbitrage Bot

Bot arbitrage multi-DEX berbasis **Morpho Blue flash loan** (fee-free), ditulis
dalam Rust menggunakan [alloy](https://github.com/alloy-rs/alloy). Mendukung
jumlah venue bebas (2..N) — misalnya Uniswap, Sushiswap, Aerodrome,
Pancakeswap — dengan memindai semua pasangan arah terurut.

## Arsitektur

```
src/
  config.rs    # konfigurasi dari env (.env)
  morpho.rs    # binding IMorphoBlue.flashLoan
  dex.rs       # pembacaan reserve Uniswap-V2 + rumus constant-product
  arbitrage.rs # deteksi peluang dua arah + simulasi profit (unit-tested)
  executor.rs  # encode ArbParams, simulasi eth_call, lalu broadcast tx
contracts/
  FlashArbitrage.sol # receiver flashloan Morpho; mengeksekusi 2 swap on-chain
```

Alur: bot membaca reserve semua pool V2 yang dikonfigurasi di `DEX_VENUES`
(format `<pair>:<router>`, dipisah koma), mensimulasikan cycle
`loan -> quote -> loan` untuk setiap pasangan venue terurut (i, j), i != j,
dan beberapa ukuran pinjaman, memilih profit maksimum, menyimulasikannya
lewat `eth_call`, lalu (jika tidak dry-run) memanggil
`FlashArbitrage.execute(...)`. Profit divalidasi on-chain (`minProfit`) dan
di-sweep ke owner. Contract tidak perlu diubah karena router diparameterkan.

## Menjalankan

```bash
cp .env.example .env   # isi RPC, key, alamat pool/router/token
cargo build
cargo test
# sekali scan:
cargo run -- once
# loop terus-menerus:
cargo run -- scan
```

`DRY_RUN=true` (default) hanya mensimulasikan tanpa broadcast transaksi.

Deploy contract dulu (mis. dengan Foundry), lalu set `ARB_CONTRACT`.

## Catatan keamanan

- Jangan pernah commit `.env` / private key.
- `FlashArbitrage.sol` dibatasi `onlyOwner`; callback dibatasi ke Morpho.
- Validasi profit on-chain mencegah eksekusi negatif akibat slippage/MEV.