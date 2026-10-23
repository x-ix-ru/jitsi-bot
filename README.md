# jitsi-bot

Rust-прототип бота для Jitsi: подключение без графического интерфейса и запись
аудио и видео каждого участника в отдельные файлы. Транспорт построен на
`gst-meet` / `lib-gst-meet` и GStreamer.

Исходники, Dockerfile, настройки и подробные команды запуска находятся в
[ветке `dev`](https://github.com/example-org/jitsi-bot/tree/dev).

Проверочная комната: `https://meet.example.org/комната`. Для этого сервера бот
использует XMPP BOSH (`/http-bind`). В коротком тесте бот появился в комнате,
получил звук и видео одного участника и создал читаемые WebM-файлы.

Проект пока проверочный: запись нескольких участников одновременно и длительная
работа ещё не испытаны. Hermes, транскрибация и функции администратора
конференции запланированы после проверки транспорта.

Для запуска:

```sh
git clone https://github.com/example-org/jitsi-bot.git
cd jitsi-bot
git switch dev
cp .env.record.example .env.record
mkdir -p recordings
docker build -f Dockerfile.bosh -t hermes-jitsi-bosh .
docker run --rm --init --env-file .env.record \
  -v "$PWD/recordings:/data/recordings" hermes-jitsi-bosh record
```

Перед записью сообщите участникам встречи о работе бота.
