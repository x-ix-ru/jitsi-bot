# jitsi-bot

Rust-прототип бота для Jitsi: подключение без графического интерфейса и запись
аудио и видео каждого участника в отдельные файлы. Транспорт построен на
`gst-meet` / `lib-gst-meet` и GStreamer.

HTTP API сервиса: `POST /api/jitsi-bot/join` запускает подключение и возвращает
ID сессии. По ID можно запросить состояние и завершить сессию.

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
cp .env.api.example .env.api
printf 'API_TOKEN=%s\n' "$(openssl rand -hex 24)" >> .env.api
mkdir -p recordings
docker build -f Dockerfile.bosh -t hermes-jitsi-bosh .
docker run --rm --init --env-file .env.api \
  -p 127.0.0.1:8080:8080 \
  -v "$PWD/recordings:/data/recordings" hermes-jitsi-bosh
```

Пример запроса после запуска (подробности в README ветки `dev`):

```sh
TOKEN=$(awk -F= '/^API_TOKEN=/{print $2}' .env.api)
curl -X POST http://127.0.0.1:8080/api/jitsi-bot/join \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"room":"комната"}'
```

Перед записью сообщите участникам встречи о работе бота.
