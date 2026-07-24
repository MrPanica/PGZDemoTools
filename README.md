# PGZ Demo Tools

Local TF2 `.dem` editor: cut and reorder ranges from one demo, inspect events and chat, export player voices, and convert POV edits to free camera.

## Windows / Linux without Docker

Requires Python 3.10+. Rust/Cargo is only needed once to build the POV and voice helpers.

```sh
python3 demo_tools.py build-helper
python3 demo_tools.py serve --host 127.0.0.1 --port 8765
```

CLI examples:

```sh
python3 demo_tools.py info game.dem
python3 demo_tools.py montage game.dem --range 30:40 --range 0:10 -o game-edit.dem
python3 demo_tools.py voice game.dem --all --format mp3 --archive
```

Ranges are written in the order supplied, including non-chronological POV and SourceTV montages. WAV/MP3 voice export requires `ffmpeg` in `PATH`. Build the standalone executable with `python -m PyInstaller --clean --noconfirm PGZDemoTools.spec`; the result is `dist/PGZDemoTools.exe` on Windows or `dist/PGZDemoTools` on Linux. Build on each target OS separately. The provided Linux x86_64 binary requires glibc 2.38 or newer.

## Docker / nginx

```sh
docker compose up -d --build
```

The service listens on `127.0.0.1:8765`. Point nginx at `http://127.0.0.1:8765`.

## Docker: nginx + PHP 8.5 without an app build step

Build the application image once, then start the stack without `build:`:

```sh
docker compose build
docker compose -f compose.nginx-php.yaml up -d
```

Nginx is available at `127.0.0.1:8771`; PHP 8.5 FPM is included for integration with an existing PHP stack. The TF2 parser remains the Python/Rust application and is proxied by nginx.

Different `.dem` files are intentionally not joined: TF2 network state becomes invalid. You may freely reorder ranges from the same demo.

---

# PGZ Demo Tools — Русский

Локальный редактор TF2 `.dem`: монтаж диапазонов одной демки, просмотр событий и чата, экспорт голосов игроков и превращение POV-монтажа в свободную камеру.

## Windows / Linux без Docker

Нужен Python 3.10+. Rust/Cargo нужен только один раз для сборки помощников POV и голосов.

```sh
python3 demo_tools.py build-helper
python3 demo_tools.py serve --host 127.0.0.1 --port 8765
```

Примеры CLI:

```sh
python3 demo_tools.py info game.dem
python3 demo_tools.py montage game.dem --range 30:40 --range 0:10 -o game-edit.dem
python3 demo_tools.py voice game.dem --all --format mp3 --archive
```

Диапазоны записываются в указанном порядке, в том числе при непоследовательном монтаже POV и SourceTV. Для экспорта голосов в WAV/MP3 нужен `ffmpeg` в `PATH`. Автономный файл собирается командой `python -m PyInstaller --clean --noconfirm PGZDemoTools.spec`: результат — `dist/PGZDemoTools.exe` в Windows или `dist/PGZDemoTools` в Linux. Сборку выполняйте отдельно в каждой целевой ОС. Готовому Linux-бинарнику x86_64 требуется glibc 2.38 или новее.

## Docker / nginx

```sh
docker compose up -d --build
```

Сервис слушает только `127.0.0.1:8765`. В nginx проксируйте нужный домен на `http://127.0.0.1:8765`.

## Docker: nginx + PHP 8.5 без сборки приложения

Один раз соберите образ приложения, затем запускайте стек без `build:`:

```sh
docker compose build
docker compose -f compose.nginx-php.yaml up -d
```

Nginx доступен на `127.0.0.1:8771`; контейнер PHP 8.5 FPM добавлен для интеграции с существующим PHP-стеком. Парсер TF2 остаётся Python/Rust-приложением и проксируется nginx.

Разные `.dem` намеренно не склеиваются: это ломает сетевое состояние TF2. Диапазоны одной демки можно переставлять в произвольном порядке.
