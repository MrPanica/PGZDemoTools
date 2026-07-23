# PGZ Demo Tools

Локальный редактор TF2 `.dem`: монтаж диапазонов одной демки, POV/free camera, события, чат и экспорт голосов.

## Windows / Linux без Docker

Нужны Python 3.10+ и Rust/Cargo для голосового помощника:

```sh
python3 demo_tools.py build-helper
python3 demo_tools.py serve --host 127.0.0.1 --port 8765
```

CLI:

```sh
python3 demo_tools.py info game.dem
python3 demo_tools.py montage game.dem --range 0:10 --range 30:40 -o game-edit.dem
python3 demo_tools.py voice game.dem --all --format mp3 --archive
```

## Docker / nginx

```sh
docker compose up -d --build
```

Сервис слушает только `127.0.0.1:8765`. В nginx проксируйте нужный домен на `http://127.0.0.1:8765`.

Разные `.dem` намеренно не склеиваются: это ломает сетевое состояние TF2. Монтаж допускает произвольный порядок диапазонов одной демки.
