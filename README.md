# PGZ Demo Tools

[![PGZ Demo Tools web editor](screenshots/Screenshot_115.png)](screenshots/README.md)

[More screenshots / Больше скриншотов](screenshots/README.md)

Editor for TF2 `.dem` files. Cut and reorder ranges from one demo, inspect events/chat, and export player voices.

## Requirements

- Windows 10/11 or Linux x86_64
- Python 3.10 or newer
- Rust stable + Cargo — required once for `build-helper` (not needed with Docker)
- `ffmpeg` in `PATH` — only for WAV/MP3 voice export
- A current browser for the web editor

The supplied Linux binary requires glibc 2.38 or newer. Build it on the target system if the server has an older glibc.

## Manual installation (Windows / Linux)

No Python packages are required. Clone or unpack the project, then build the two helpers once:

```sh
git clone https://github.com/MrPanica/PGZDemoTools.git
cd PGZDemoTools
python3 demo_tools.py build-helper
python3 demo_tools.py serve --host 127.0.0.1 --port 8765
```

Open `http://127.0.0.1:8765`. Web sessions are kept in `.work` next to the script. Use `--workspace PATH` or `PGZ_DEMO_WORKSPACE` to store them elsewhere.

CLI examples:

```sh
python3 demo_tools.py info game.dem
python3 demo_tools.py montage game.dem --range 30:40 --range 0:10 -o game-edit.dem
python3 demo_tools.py voice game.dem --all --format mp3 --archive
```

Ranges keep the order supplied. They may be non-chronological, but they must belong to the same demo. POV edits retain the original recording player.

Build the standalone program separately on each target OS:

```sh
python -m PyInstaller --clean --noconfirm PGZDemoTools.spec
```

The output is `dist/PGZDemoTools.exe` on Windows and `dist/PGZDemoTools` on Linux.

## Manual nginx setup (Linux)

Run the editor locally on the server and proxy nginx to it:

```sh
python3 demo_tools.py serve --host 127.0.0.1 --port 8765 --no-browser
```

Use this nginx server block, replacing `demo.example.com`:

```nginx
server {
    listen 80;
    server_name demo.example.com;

    client_max_body_size 2g;

    location / {
        proxy_pass http://127.0.0.1:8765;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

Save it in `/etc/nginx/sites-available/pgz-demo-tools`, enable it, then test and reload nginx:

```sh
sudo ln -s /etc/nginx/sites-available/pgz-demo-tools /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
```

For a persistent service, use your process manager (for example systemd) to run the same `serve` command. PHP is not needed for the editor itself.

## Docker

```sh
docker compose up -d --build
```

The app listens on `127.0.0.1:8765`; proxy nginx to that address.

## Docker: nginx + PHP 8.5

The release archive contains the application, nginx, and PHP 8.5 FPM containers. Docker Engine with Compose is the only requirement:

```sh
unzip PGZDemoTools-nginx-php-*.zip
cd PGZDemoTools-nginx-php
docker compose build
docker compose -f compose.nginx-php.yaml up -d
```

Open `http://SERVER_IP:8771`. Stop it with `docker compose -f compose.nginx-php.yaml down`. PHP is included only for an existing PHP site; the editor is the Python/Rust service behind nginx.

---

# PGZ Demo Tools — Русский

Редактор TF2 `.dem`: нарезка и перестановка отрезков одной демки, просмотр событий/чата и экспорт голосов игроков.

## Требования

- Windows 10/11 или Linux x86_64
- Python 3.10 или новее
- Rust stable и Cargo — один раз для `build-helper` (в Docker не нужны)
- `ffmpeg` в `PATH` — только для экспорта голосов в WAV/MP3
- Современный браузер для веб-редактора

Готовому Linux-бинарнику нужна glibc 2.38 или новее. На более старом сервере соберите бинарник на самом сервере.

## Ручная установка (Windows / Linux)

Python-пакеты не нужны. Клонируйте или распакуйте проект и один раз соберите два помощника:

```sh
git clone https://github.com/MrPanica/PGZDemoTools.git
cd PGZDemoTools
python3 demo_tools.py build-helper
python3 demo_tools.py serve --host 127.0.0.1 --port 8765
```

Откройте `http://127.0.0.1:8765`. Веб-сессии лежат в `.work` рядом со скриптом. Путь меняется через `--workspace PATH` или `PGZ_DEMO_WORKSPACE`.

Примеры CLI:

```sh
python3 demo_tools.py info game.dem
python3 demo_tools.py montage game.dem --range 30:40 --range 0:10 -o game-edit.dem
python3 demo_tools.py voice game.dem --all --format mp3 --archive
```

Отрезки записываются в указанном порядке. Они могут идти не по хронологии, но должны принадлежать одной демке. В POV-монтаже сохраняется исходный игрок записи.

Автономный файл собирайте отдельно в каждой целевой ОС:

```sh
python -m PyInstaller --clean --noconfirm PGZDemoTools.spec
```

Результат: `dist/PGZDemoTools.exe` в Windows и `dist/PGZDemoTools` в Linux.

## Ручная установка nginx (Linux)

Запустите редактор локально на сервере и проксируйте к нему nginx:

```sh
python3 demo_tools.py serve --host 127.0.0.1 --port 8765 --no-browser
```

Создайте конфигурацию nginx, заменив `demo.example.com`:

```nginx
server {
    listen 80;
    server_name demo.example.com;

    client_max_body_size 2g;

    location / {
        proxy_pass http://127.0.0.1:8765;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

Сохраните файл как `/etc/nginx/sites-available/pgz-demo-tools`, включите его, затем проверьте и перезагрузите nginx:

```sh
sudo ln -s /etc/nginx/sites-available/pgz-demo-tools /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
```

Для постоянного запуска используйте менеджер процессов (например systemd) с той же командой `serve`. PHP самому редактору не нужен.

## Docker

```sh
docker compose up -d --build
```

Приложение слушает `127.0.0.1:8765`; проксируйте nginx на этот адрес.

## Docker: nginx + PHP 8.5

В архиве есть контейнеры приложения, nginx и PHP 8.5 FPM. Нужен только Docker Engine с Compose:

```sh
unzip PGZDemoTools-nginx-php-*.zip
cd PGZDemoTools-nginx-php
docker compose build
docker compose -f compose.nginx-php.yaml up -d
```

Откройте `http://SERVER_IP:8771`. Остановка: `docker compose -f compose.nginx-php.yaml down`. PHP добавлен только для существующего PHP-сайта; редактор — Python/Rust-сервис за nginx.
