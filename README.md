# Credits Exporter

```
Alice follows a white rabbit with pink eyes because she saw the rabbit checking a pocket watch.
She chases the rabbit, and it bounds into a rabbit hole. (c) Charles Lutwidge Dodgson
```

```bash
cargo build --release
```
```sh
[Unit]
Description=Solana Credits Exporter
After=network.target

[Service]
ExecStart=/home/exporter/bin/solana-credits-exporter

WorkingDirectory=/home/exporter

User=exporter
Group=exporter

Restart=on-failure

RestartSec=5

StandardOutput=journal
StandardError=journal

LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```
