# Morpho Flashloan Arbitrage Bot

Bot arbitrage multi-DEX berbasis **Morpho Blue flash loan** (fee-free), ditulis
dalam Rust menggunakan [alloy](https://github.com/alloy-rs/alloy). Mendukung
jumlah venue bebas (2..N) — Uniswap V2, Sushiswap V2, Pancakeswap V2,
Aerodrome, dan Uniswap V3 — dengan memindai semua pasangan arah terurut.

## Daftar Isi

1. [Arsitektur](#arsitektur)
2. [Prasyarat & Instalasi](#prasyarat--instalasi)
3. [Build & Test](#3-build--test)
4. [Deploy Kontrak FlashArbitrage](#4-deploy-kontrak-flasharbitrage)
5. [Konfigurasi (.env)](#5-konfigurasi-env)
6. [Menjalankan Bot](#6-menjalankan-bot)
7. [Mode Produksi](#7-mode-produksi)
8. [Troubleshooting](#8-troubleshooting)
9. [Catatan Keamanan](#catatan-keamanan)

## Arsitektur

```
src/
  config.rs    # konfigurasi dari env (.env) + validasi fail-fast
  dex.rs       # reserve V2/Aero, QuoterV2 untuk V3, batch JSON-RPC
  arbitrage.rs # pencarian peluang dari quote riil per ukuran (unit-tested)
  executor.rs  # encode ArbParams, gas estimate (= simulasi), broadcast
contracts/
  FlashArbitrage.sol # receiver flashloan Morpho; mengeksekusi 2 swap on-chain
```

Alur satu iterasi scan:

1. **Batch RPC fase 1** — `getReserves` untuk setiap venue V2/Aero, satu call
   **QuoterV2** (`quoteExactInputSingle`) untuk setiap venue V3 x setiap
   ukuran pinjaman (leg 1: loan → quote), plus `eth_gasPrice`. Semua dalam
   SATU batch JSON-RPC, dipin ke nomor block eksplisit.
2. **Batch RPC fase 2** — leg 2 (quote → loan) untuk venue V3, karena
   inputnya baru diketahui setelah leg 1 dihargai. Dipin ke block yang sama
   dengan fase 1 agar kedua leg dihargai pada state chain yang konsisten.
   Venue V2 dihitung lokal dengan rumus constant-product yang eksak.
3. **Pencarian peluang** — semua pasangan venue terurut (i, j), i != j, dan
   semua ukuran pinjaman; dipilih profit kotor maksimum.
4. **Filter gas** — `eth_estimateGas` untuk `execute`, dikali gas price.
   Karena `LOAN_TOKEN` diwajibkan sama dengan wrapped native, estimasi wei
   langsung sebanding dengan profit. Profit bersih harus >= `MIN_PROFIT`.
   Call ini sekaligus berfungsi sebagai gate simulasi: estimateGas
   mengeksekusi tx penuh, jadi opportunity yang akan revert ditolak di
   sini tanpa biaya.
5. **Broadcast fire-and-forget** — tx dikirim dan bot langsung kembali
   memindai; receipt dipantau di background task. Selama satu tx masih
   pending inklusi, bot menahan diri dari broadcast duplikat (flag
   in-flight yang dibersihkan oleh watcher receipt), sehingga satu
   peluang tidak dikejar dengan banyak tx yang saling bersaing.

Pricing V3 memakai **QuoterV2**, yaitu traversal tick/liquidity riil yang
dilakukan on-chain via `eth_call` — bukan estimasi spot price — sehingga
output untuk trade besar akurat dan pemilihan ukuran pinjaman benar.

Kontrak mendukung tiga keluarga router lewat `SwapLeg.kind`:

- `0` = Uniswap-V2-style (`address[] path`) — Uniswap V2, Sushiswap V2,
  Pancakeswap V2.
- `1` = Aerodrome-style (`Route[]{from,to,stable,factory}`).
- `2` = Uniswap-V3-style (`exactInputSingle` dengan `feeTier`).

Setiap leg membawa `minOut` (dari quote riil dikali `1 - SLIPPAGE_BPS`)
untuk membatasi drift harga antara simulasi dan inklusi; toleransi leg B
dikompound dua kali karena input-nya adalah output aktual leg A; cek
`minProfit` on-chain tetap menjadi backstop terakhir.

Catatan MEV di Base: sequencer terpusat dengan mempool privat, jadi tidak
ada sandwich/frontrunning. Risiko nyata adalah kalah balapan dengan bot arb
lain — tx yang kalah revert dan rugi gas. Mitigasi: RPC latensi rendah,
`WSS_URL` untuk scanning per-block, dan estimateGas sebagai gate simulasi
tepat sebelum kirim.

Catatan: pool Aerodrome **stable** memakai kurva x^3y+y^3x, bukan constant
product; simulasi off-chain bot ini hanya akurat untuk pool volatile (vAMM).

## Prasyarat & Instalasi

### 1.1. Dependensi sistem (Debian/Ubuntu)

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev curl git
```

Untuk macOS:

```bash
xcode-select --install
brew install pkg-config openssl
```

### 1.2. Install Rust (via rustup)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# verifikasi
rustc --version   # minimal 1.85 (edition 2021 + alloy v1)
cargo --version
```

Agar `cargo` selalu ada di PATH untuk shell berikutnya:

```bash
echo 'source "$HOME/.cargo/env"' >> ~/.bashrc
```

### 1.3. Install Foundry (untuk deploy kontrak)

```bash
curl -L https://foundry.paradigm.xyz | bash
source ~/.bashrc   # atau buka shell baru
foundryup

# verifikasi
forge --version
cast --version
```

### 1.4. Siapkan RPC endpoint

Bot butuh RPC untuk chain target (contoh di sini: **Base mainnet**):

- HTTP RPC (wajib): daftar gratis di [Alchemy](https://www.alchemy.com/),
  [Chainstack](https://chainstack.com/), atau [QuickNode](https://www.quicknode.com/),
  lalu buat app untuk jaringan Base dan salin URL HTTPS-nya.
- WebSocket RPC (opsional, direkomendasikan): dari provider yang sama,
  salin URL `wss://`-nya untuk scanning event-driven per block.

Untuk coba-coba, RPC publik `https://mainnet.base.org` bisa dipakai, tapi
rate-limit-nya ketat — tidak cocok untuk produksi.

### 1.5. Siapkan wallet khusus bot

Buat private key BARU yang khusus dipakai bot (jangan pakai wallet utama):

```bash
cast wallet new
```

Simpan output `Private Key` dan `Address`-nya. Wallet ini akan menjadi
**owner** kontrak (satu-satunya yang bisa memanggil `execute`/`sweep`) dan
perlu saldo ETH kecil di Base untuk gas (~0.001 ETH cukup untuk puluhan tx).

### 1.6. Clone repository

```bash
git clone <URL-REPO-INI> morpho-arbitrage-rust
cd morpho-arbitrage-rust
```

## 3. Build & Test

```bash
# compile (pertama kali akan mengunduh dependensi, bisa beberapa menit)
cargo build

# jalankan seluruh unit test (rumus swap, pencarian peluang, ABI encoding)
cargo test

# build binary release yang dioptimasi (untuk menjalankan bot sungguhan)
cargo build --release
```

Binary release tersedia di `./target/release/morpho-arbitrage-bot`.

## 4. Deploy Kontrak FlashArbitrage

Kontrak `contracts/FlashArbitrage.sol` adalah penerima flash loan yang
mengeksekusi kedua swap on-chain. Deploy dengan Foundry:

```bash
# dari root repo
forge create contracts/FlashArbitrage.sol:FlashArbitrage --rpc-url https://base-mainnet.g.alchemy.com/v2/KEY_ANDA --private-key 0xPRIVATE_KEY_BOT --broadcast --constructor-args 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
```

- `0xBBBB...FFCb` adalah alamat **Morpho Blue** (sama di Base & Ethereum
  mainnet).
- **`--constructor-args` harus paling akhir** — pada beberapa versi forge
  flag ini menelan semua argumen setelahnya, sehingga flag lain yang
  diletakkan sesudahnya salah dibaca sebagai argumen konstruktor (error
  "Constructor argument count mismatch").
- Output akan menampilkan `Deployed to: 0x...` — simpan alamat ini sebagai
  `ARB_CONTRACT` di `.env`.
- Deployer otomatis menjadi `owner` kontrak; hanya owner yang bisa
  `execute` dan `sweep` profit.

Alternatif tanpa `--private-key` di command line (lebih aman, interaktif),
dengan verifikasi source via Sourcify (tanpa API key):

```bash
forge create contracts/FlashArbitrage.sol:FlashArbitrage --rpc-url $RPC_URL --interactive --broadcast --verify --verifier sourcify --chain 8453 --constructor-args 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
```

## 5. Konfigurasi (.env)

```bash
cp .env.example .env
nano .env   # atau editor favorit Anda
```

Variabel yang tersedia:

| Variabel | Wajib | Keterangan |
|---|---|---|
| `RPC_URL` | Ya | HTTP RPC endpoint (Base). |
| `WSS_URL` | Tidak | WebSocket endpoint; mengaktifkan scanning per-block. Kosongkan untuk mode polling. |
| `PRIVATE_KEY` | Ya | Private key wallet bot (= owner kontrak). **Jangan pernah commit.** |
| `MORPHO_ADDRESS` | Ya | Morpho Blue: `0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb`. |
| `ARB_CONTRACT` | Ya | Alamat kontrak hasil deploy langkah 4. |
| `LOAN_TOKEN` | Ya | Token yang dipinjam + unit profit. **Harus sama dengan `WRAPPED_NATIVE`** (gas dibayar ETH; hanya loan native yang bisa memperhitungkan gas secara eksak). |
| `QUOTE_TOKEN` | Ya | Token perantara cycle (mis. USDC `0x8335...2913`). |
| `WRAPPED_NATIVE` | Tidak | Default WETH Base. `LOAN_TOKEN` wajib menyamainya. |
| `DEX_VENUES` | Ya | Daftar venue, format di bawah. |
| `LOAN_AMOUNTS` | Tidak | Ukuran pinjaman yang diuji, koma-separated (base unit). Default `1000000000000000000` (1 token). |
| `MIN_PROFIT` | Ya* | Profit bersih minimum (base unit loan token). **Harus > 0 jika `DRY_RUN=false`**. |
| `SLIPPAGE_BPS` | Tidak | Toleransi slippage per leg dalam bps. Default `50` (0.5%). |
| `GAS_PRICE_WEI` | Tidak | Override gas price; default diambil on-chain. |
| `QUOTER_V2` | Tidak | Alamat QuoterV2 untuk pricing V3. Default `0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a` (**khusus Base**; chain lain wajib diisi, mis. Ethereum mainnet `0x61fFE014bA17989E743c5F6cB21bF9697530B21e`). |
| `POLL_INTERVAL_MS` | Tidak | Interval polling untuk mode `scan` tanpa WSS. Default `500`. |
| `DRY_RUN` | Tidak | Default `true` = hanya simulasi, tidak broadcast. Set `false` untuk live trading. |

Format `DEX_VENUES` (koma-separated):

```
<POOL>:<ROUTER>[:<kind>[:<fee_bps>[:<factory>[:<stable>[:<fee_tier>[:<pool_id>]]]]]
```

- `POOL` = alamat pool/pair, atau `auto` untuk resolve dari `factory` saat startup.
- `kind` = `v2` (default) | `aero` | `v3` (`v4` belum didukung).
- `fee_bps` = fee pool dalam basis point (default 30; untuk V2/Aero).
- `fee_tier` = fee tier Uniswap V3 dalam hundredths of a bip (500/3000/10000).
- `stable` = `true` untuk pool stable Aerodrome.

Alamat terverifikasi di Base:

```
Uniswap V2  router  0x4752ba5DBc23f44D87826276BF6Fd6b1c372aD24
            factory 0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6
Aerodrome   router  0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874e43
            factory 0x420DD381b31aEf6683db6B902084cB0FFECe40Da
Uniswap V3  router  0x2626664c2603336E57B271c5C0b26F421741e481
            factory 0x33128a8fC17869897dcE68Ed026d694621f6FDfD
QuoterV2            0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a
Morpho Blue         0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
WETH                0x4200000000000000000000000000000000000006
USDC                0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913
```

Contoh WETH/USDC di 3 venue sekaligus (pool auto-resolve):

```bash
DEX_VENUES=auto:0x4752ba5DBc23f44D87826276BF6Fd6b1c372aD24:v2:30:0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6,auto:0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874e43:aero:30:0x420DD381b31aEf6683db6B902084cB0FFECe40Da,auto:0x2626664c2603336E57B271c5C0b26F421741e481:v3:30:0x33128a8fC17869897dcE68Ed026d694621f6FDfD::3000
```

## 6. Menjalankan Bot

Selalu mulai dengan `DRY_RUN=true` (default) sampai Anda yakin konfigurasi
benar — bot hanya mensimulasikan dan tidak mengirim transaksi apa pun.

```bash
# uji sekali scan lalu keluar (cara tercepat memvalidasi .env)
cargo run --release -- once

# loop terus-menerus (polling, atau per-block jika WSS_URL diisi)
cargo run --release -- scan
```

Log yang sehat terlihat seperti:

```
INFO bot configured morpho=0xBBBB... arb_contract=0x... venues=3 dry_run=true
INFO pool auto-resolved venue=0 pool=0x... kind=UniswapV2
INFO no profitable opportunity
```

Saat peluang ditemukan:

```
INFO opportunity found first=0 second=2 loan=... gross=... gas=... net=...
INFO dry-run enabled; skipping broadcast
```

### Go-live (broadcast sungguhan)

1. Pastikan `MIN_PROFIT` > 0 (bot menolak start jika 0 saat live).
2. Pastikan wallet owner punya saldo ETH untuk gas.
3. Set `DRY_RUN=false` di `.env`.
4. Jalankan `cargo run --release -- scan`.

Mode live: bot melakukan gas-estimate (= simulasi penuh) tepat sebelum
broadcast; tx dikirim **fire-and-forget** dan receipt-nya dilaporkan di log
oleh background task (`arbitrage transaction confirmed` / `reverted`), jadi
scanning tidak pernah berhenti menunggu inklusi.

## 7. Mode Produksi

Menjalankan sebagai service systemd:

```ini
# /etc/systemd/system/morpho-arb.service
[Unit]
Description=Morpho arbitrage bot
After=network-online.target

[Service]
WorkingDirectory=/opt/morpho-arbitrage-rust
ExecStart=/opt/morpho-arbitrage-rust/target/release/morpho-arbitrage-bot scan
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now morpho-arb
journalctl -u morpho-arb -f   # pantau log
```

Tips produksi:

- Pakai RPC berbayar dengan latensi rendah; isi `WSS_URL` agar scan terpicu
  setiap block, bukan menunggu interval polling.
- `RUST_LOG=debug` untuk diagnosa lebih verbose.
- Profit terkumpul di kontrak; tarik berkala dengan:
  `cast send $ARB_CONTRACT "sweep(address)" $LOAN_TOKEN --rpc-url $RPC_URL --private-key $PRIVATE_KEY`

## 8. Troubleshooting

| Gejala | Penyebab umum | Solusi |
|---|---|---|
| `missing env var ...` | `.env` belum dibuat/diisi | `cp .env.example .env` lalu isi |
| `venue N pool ... does not contain loan/quote token` | pool bukan pasangan LOAN/QUOTE | periksa alamat pool di `DEX_VENUES` |
| `V3 venue returned no usable quotes` | fee_tier salah / pool tipis / `QUOTER_V2` salah chain | cek `fee_tier` cocok dengan pool (500/3000/10000); cek alamat QuoterV2 |
| `MIN_PROFIT must be greater than zero` | live mode dengan floor 0 | set `MIN_PROFIT` > 0 |
| `opportunity filtered out by gas cost` terus-menerus | spread < biaya gas | normal; naikkan `LOAN_AMOUNTS` atau tunggu volatilitas |
| `arbitrage transaction reverted on-chain` | kalah balapan bot lain / harga drift | normal sesekali; pertimbangkan RPC lebih cepat |
| batch RPC gagal | RPC publik kena rate-limit | pakai endpoint berbayar |

## Catatan Keamanan

- Jangan pernah commit `.env` / private key (`.env` sudah ada di `.gitignore`).
- Pakai wallet khusus bot dengan saldo minimal; profit tersimpan di kontrak
  dan hanya bisa di-`sweep` oleh owner.
- `FlashArbitrage.sol` dibatasi `onlyOwner`; callback dibatasi ke Morpho.
- Pertahanan berlapis: `minOut` per leg → `minProfit` on-chain →
  `eth_estimateGas` sebagai gate simulasi. Kegagalan terburuk adalah rugi gas,
  bukan kehilangan principal (flash loan yang gagal otomatis revert).
