# PGZ Demo Tools

[![PGZ Demo Tools web editor](screenshots/Screenshot_115.png)](screenshots/README.md)

[More screenshots / Больше скриншотов](screenshots/README.md)

Editor for TF2 `.dem` files. Cut and reorder ranges from one demo, inspect events/chat, and export player voices.

## Requirements

- Windows 10/11 or Linux x86_64
- Rust 1.85 or newer with Cargo (not needed with Docker or a release binary)
- `ffmpeg` in `PATH` — only for WAV/MP3 voice export
- A current browser — only for the web editor

The supplied Linux binary targets Debian 12 / glibc 2.36. Build it on the target system if the server has an older glibc.

## Manual installation (Windows / Linux)

Clone or unpack the project, then build the CLI/web executable and the native desktop companion:

```sh
git clone https://github.com/MrPanica/PGZDemoTools.git
cd PGZDemoTools
cargo build --release
./target/release/PGZDemoTools serve --host 127.0.0.1 --port 8765
```

On Windows, use `target\release\PGZDemoTools.exe` for CLI/web and `target\release\PGZDemoToolsDesktop.exe` for the native desktop editor. The desktop application is a Rust window with native file dialogs; it does not embed Chromium or Electron. It follows the system theme and language by default, with overrides in Settings. Web sessions are kept in `.work` next to the executable. Use `--workspace PATH` or `PGZ_DEMO_WORKSPACE` to store them elsewhere.

Montage processing runs as a background Rust job, so a reverse proxy does not have to keep one long edit request open. The web progress ring is based on the actual packet position read by the POV/SourceTV cutter; the final download segment is based on transferred bytes. The browser waits up to 300 seconds for progress by default; set `PGZ_DEMO_PROGRESS_TIMEOUT_SECONDS` to a value from 30 to 3600 to change that UI limit. For example:

```sh
PGZ_DEMO_PROGRESS_TIMEOUT_SECONDS=600 PGZDemoTools serve --no-browser
```

CLI examples:

```sh
PGZDemoTools info game.dem
PGZDemoTools montage game.dem --range 30:40 --range 0:10 -o game-edit.dem
PGZDemoTools voice game.dem --all --format mp3 --archive
```

Ranges keep the order supplied. They may be non-chronological, but they must belong to the same demo. POV edits retain the original recording player.

For POV demos, the web editor can apply **Unlock free camera** after the montage is built. The resulting file is a SourceTV spectator demo with a detached roaming camera. The option is intentionally hidden for SourceTV inputs, and the output size normally stays close to the ordinary montage.

The HTML/JavaScript editor and both POV/SourceTV cutters are embedded in that executable. `build-helper` remains a compatibility no-op.

## Manual nginx setup (Linux)

Run the editor locally on the server and proxy nginx to it:

```sh
PGZDemoTools serve --host 127.0.0.1 --port 8765 --no-browser
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
        proxy_read_timeout 900s;
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

Open `http://SERVER_IP:8771`. Stop it with `docker compose -f compose.nginx-php.yaml down`. PHP is included only for an existing PHP site; the editor is the Rust service behind nginx.

## Third-party components

- [demostf/parser](https://github.com/demostf/parser) — the bundled and locally patched `tf-demo-parser` fork used for TF2 packet parsing.
- [tiny-http](https://github.com/tiny-http/tiny-http) — the embedded local HTTP server.
- [clap](https://github.com/clap-rs/clap) and [Serde](https://github.com/serde-rs/serde) — CLI and JSON serialization.
- [zip-rs](https://github.com/zip-rs/zip2) — voice archive creation.
- [egui / eframe](https://github.com/emilk/egui) — native desktop UI renderer, not a browser engine.
- [rfd](https://github.com/PolyMeilex/rfd) — native open/save file dialogs.
- [sys-locale](https://github.com/1Password/sys-locale) — system-language detection for the desktop UI.
- [lamejs](https://github.com/zhuker/lamejs) / [LAME](https://github.com/lameproject/lame) — browser-side MP3 encoding; the bundled license notice is in `LAMEJS-LICENSE.txt`.
- [FFmpeg](https://github.com/FFmpeg/FFmpeg) — optional external WAV/MP3 converter used only by the CLI.

The complete direct Rust dependency list is in `Cargo.toml`; exact resolved versions, including transitive crates, are in `Cargo.lock`.

---

# PGZ Demo Tools — Русский

Редактор TF2 `.dem`: нарезка и перестановка отрезков одной демки, просмотр событий/чата и экспорт голосов игроков.

## Требования

- Windows 10/11 или Linux x86_64
- Rust 1.85 или новее с Cargo (не нужен при запуске через Docker или готовый бинарник)
- `ffmpeg` в `PATH` — только для экспорта голосов в WAV/MP3
- Современный браузер — только для веб-редактора

Готовый Linux-бинарник рассчитан на Debian 12 / glibc 2.36. На более старом сервере соберите бинарник на самом сервере.

## Ручная установка (Windows / Linux)

Клонируйте или распакуйте проект и соберите CLI/веб-бинарник и нативное desktop-приложение:

```sh
git clone https://github.com/MrPanica/PGZDemoTools.git
cd PGZDemoTools
cargo build --release
./target/release/PGZDemoTools serve --host 127.0.0.1 --port 8765
```

В Windows используйте `target\release\PGZDemoTools.exe` для CLI/веба и `target\release\PGZDemoToolsDesktop.exe` для нативного desktop-редактора. Desktop-приложение — это Rust-окно с нативными файловыми диалогами, без Chromium и Electron. По умолчанию оно подхватывает тему и язык системы; их можно изменить в «Настройках». Веб-сессии лежат в `.work` рядом с исполняемым файлом. Путь меняется через `--workspace PATH` или `PGZ_DEMO_WORKSPACE`.

Монтаж обрабатывается фоновой Rust-задачей, поэтому reverse proxy не обязан держать один долгий запрос нарезки. Кольцо прогресса веба строится по фактической позиции пакетов, которую уже читает резак POV/SourceTV; финальная часть скачивания считается по переданным байтам. По умолчанию браузер ждёт прогресс до 300 секунд; лимит интерфейса меняется переменной `PGZ_DEMO_PROGRESS_TIMEOUT_SECONDS` в пределах 30–3600 секунд. Например:

```sh
PGZ_DEMO_PROGRESS_TIMEOUT_SECONDS=600 PGZDemoTools serve --no-browser
```

Примеры CLI:

```sh
PGZDemoTools info game.dem
PGZDemoTools montage game.dem --range 30:40 --range 0:10 -o game-edit.dem
PGZDemoTools voice game.dem --all --format mp3 --archive
```

Отрезки записываются в указанном порядке. Они могут идти не по хронологии, но должны принадлежать одной демке. В POV-монтаже сохраняется исходный игрок записи.

Для POV-демок в веб-редакторе есть флажок **«Разблокировать свободную камеру»**. Он применяется после сборки монтажа и создаёт SourceTV-демку с отвязанной свободной камерой наблюдателя. Для исходных SourceTV флажок скрыт; размер результата обычно остаётся близким к обычному монтажу.

HTML/JavaScript-интерфейс и рабочие обработчики POV/SourceTV встроены в этот файл. `build-helper` оставлен совместимой no-op-командой.

## Ручная установка nginx (Linux)

Запустите редактор локально на сервере и проксируйте к нему nginx:

```sh
PGZDemoTools serve --host 127.0.0.1 --port 8765 --no-browser
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
        proxy_read_timeout 900s;
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

Откройте `http://SERVER_IP:8771`. Остановка: `docker compose -f compose.nginx-php.yaml down`. PHP добавлен только для существующего PHP-сайта; редактор — Rust-сервис за nginx.

## Сторонние компоненты

- [demostf/parser](https://github.com/demostf/parser) — встроенный и локально исправленный форк `tf-demo-parser` для разбора пакетов TF2.
- [tiny-http](https://github.com/tiny-http/tiny-http) — встроенный локальный HTTP-сервер.
- [clap](https://github.com/clap-rs/clap) и [Serde](https://github.com/serde-rs/serde) — CLI и сериализация JSON.
- [zip-rs](https://github.com/zip-rs/zip2) — создание ZIP-архивов с голосами.
- [egui / eframe](https://github.com/emilk/egui) — нативный renderer desktop-интерфейса, не браузерный движок.
- [rfd](https://github.com/PolyMeilex/rfd) — нативные диалоги открытия и сохранения файлов.
- [sys-locale](https://github.com/1Password/sys-locale) — определение системного языка для desktop-интерфейса.
- [lamejs](https://github.com/zhuker/lamejs) / [LAME](https://github.com/lameproject/lame) — кодирование MP3 в браузере; уведомление о лицензии лежит в `LAMEJS-LICENSE.txt`.
- [FFmpeg](https://github.com/FFmpeg/FFmpeg) — необязательный внешний конвертер WAV/MP3, используемый только CLI.

Полный список прямых Rust-зависимостей находится в `Cargo.toml`; точные версии, включая транзитивные зависимости, зафиксированы в `Cargo.lock`.
