# Deployment

## Build the binary

```sh
cargo build --release -p lm-serve
# binary at target/release/lm-serve (~4 MB stripped)
```

Copy it to the server:

```sh
scp target/release/lm-serve user@host:/usr/local/bin/lm-serve
scp roads.pmtiles user@host:/var/lib/light-maps/roads.pmtiles
```

---

## systemd

A ready-to-use unit file is at [`deploy/light-maps.service`](../deploy/light-maps.service).

```sh
# create a dedicated user
useradd -r -s /bin/false light-maps
mkdir -p /var/lib/light-maps

# install
cp deploy/light-maps.service /etc/systemd/system/light-maps.service
systemctl daemon-reload
systemctl enable --now light-maps

# check
systemctl status light-maps
journalctl -u light-maps -f
```

Edit the `ExecStart` line in the unit file to match your paths and flags. To pass a secret API key without it appearing in `ps`, add an `EnvironmentFile`:

```ini
EnvironmentFile=/etc/light-maps/env
```

```sh
# /etc/light-maps/env  (chmod 600, owned by root)
LM_API_KEY=your-secret-token
```

Then reference it in `ExecStart`:

```ini
ExecStart=/usr/local/bin/lm-serve /var/lib/light-maps/roads.pmtiles \
    --addr 127.0.0.1:3000 \
    --api-key $LM_API_KEY
```

---

## Docker

A two-stage Dockerfile is at [`deploy/Dockerfile`](../deploy/Dockerfile).

```sh
# build
docker build -f deploy/Dockerfile -t light-maps .

# run — mount your .pmtiles files into /data
docker run -d \
  -p 3000:3000 \
  -v /path/to/tiles:/data:ro \
  light-maps \
  /usr/local/bin/lm-serve /data/roads.pmtiles --addr 0.0.0.0:3000 --cors "*"
```

The image is ~20 MB (debian:bookworm-slim base, stripped binary, no shell in PATH by default).

---

## Behind nginx

`lm-serve` is designed to sit behind a reverse proxy. Set `--base-url` to the public URL so TileJSON contains the correct tile URLs.

```nginx
server {
    listen 443 ssl;
    server_name maps.example.com;

    location / {
        proxy_pass         http://127.0.0.1:3000;
        proxy_set_header   Host $host;
        proxy_set_header   X-Real-IP $remote_addr;

        # Pass pre-compressed tiles straight through.
        proxy_set_header   Accept-Encoding "";
        gzip off;
    }
}
```

```sh
lm-serve roads.pmtiles \
  --addr 127.0.0.1:3000 \
  --base-url https://maps.example.com \
  --cors https://yoursite.com
```

---

## Behind Caddy

```caddy
maps.example.com {
    reverse_proxy localhost:3000
}
```

Caddy handles TLS automatically. Pass `--base-url https://maps.example.com` to `lm-serve`.

---

## Health check

`GET /healthz` returns `200 ok` with no body. Use it for load-balancer or Docker health checks:

```sh
curl -sf http://localhost:3000/healthz
```

```dockerfile
HEALTHCHECK --interval=30s CMD curl -sf http://localhost:3000/healthz || exit 1
```
