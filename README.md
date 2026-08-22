# Morpho Flashloan Arbitrage Bot

Bot arbitrage dua-DEX berbasis **Morpho Blue flash loan** (fee-free), ditulis
dalam Rust menggunakan [alloy](https://github.com/alloy-rs/alloy).

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

Alur: bot membaca reserve dua pool V2, mensimulasikan cycle
`loan -> quote -> loan` untuk dua arah dan beberapa ukuran pinjaman, memilih
profit maksimum, menyimulasikannya lewat `eth_call`, lalu (jika tidak dry-run)
memanggil `FlashArbitrage.execute(...)`. Profit divalidasi on-chain
(`minProfit`) dan di-sweep ke owner.

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