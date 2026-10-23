# hermes-jitsi-bridge

Отдельный Rust-сервис для проверки Jitsi-транспорта и записи участников.
Для `meet.example.org` используется BOSH, потому что `/xmpp-websocket` на этом
сервере возвращает HTTP 404. Основой служит Rust-библиотека `lib-gst-meet` с
небольшим [патчем BOSH](patches/gst-meet-bosh.patch) поверх закреплённой ревизии
`2947267f8d2ca780b010cd4f4c38e0f41cda0009`. Медиа обрабатывает GStreamer.
Node.js и браузер внутри контейнера не используются.
Сервис запускает отдельный Rust-бинарник `gst-meet`, использующий `lib-gst-meet`;
библиотека пока не встроена напрямую в процесс сервиса.

## HTTP API

Сборка и запуск сервиса:

```sh
docker build -f Dockerfile.bosh -t hermes-jitsi-bosh .
cp .env.api.example .env.api
printf 'API_TOKEN=%s\n' "$(openssl rand -hex 24)" >> .env.api
mkdir -p recordings
docker run --rm --init --env-file .env.api \
  -p 127.0.0.1:8080:8080 \
  -v "$PWD/recordings:/data/recordings" hermes-jitsi-bosh
```

Пример вызова из другого терминала (токен взять из `.env.api`):

```sh
TOKEN=$(awk -F= '/^API_TOKEN=/{print $2}' .env.api)
curl -sS -X POST http://127.0.0.1:8080/api/jitsi-bot/join \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"room":"комната"}'
```

Ответ `202 Accepted`:

```json
{"session_id":"550e8400-e29b-41d4-a716-446655440000","room":"комната","status":"starting","recording_dir":"/data/recordings/550e8400-e29b-41d4-a716-446655440000","exit_code":null}
```

ID выше приведён для примера. `starting` означает, что процесс запущен, но
вход в комнату ещё не подтверждён. Когда установлено медиасоединение, состояние
становится `joined`. Если процесс завершился, состояние будет `exited`.

```sh
SESSION_ID=550e8400-e29b-41d4-a716-446655440000 # замените ID из ответа join
curl -sS http://127.0.0.1:8080/api/jitsi-bot/sessions/$SESSION_ID \
  -H "Authorization: Bearer $TOKEN"
curl -sS -X POST http://127.0.0.1:8080/api/jitsi-bot/sessions/$SESSION_ID/leave \
  -H "Authorization: Bearer $TOKEN"
```

Для `join` можно передать `{}`: тогда используются `JITSI_ROOM` и `JITSI_NICK`
из конфигурации. Каждый вызов создаёт отдельный процесс и каталог записи с
ID сессии. Максимум — 10 активных сессий. Состояния сессий хранятся в памяти
и исчезают после перезапуска сервиса; записи остаются в каталоге. API требует
секретный `API_TOKEN` (не менее 16 байт). В примере порт доступен только с
машины, где запущен Docker.

## Запись комнаты «комната»

```sh
docker build -f Dockerfile.bosh -t hermes-jitsi-bosh .
cp .env.record.example .env.record
mkdir -p recordings
docker run --rm --init --env-file .env.record \
  -v "$PWD/recordings:/data/recordings" hermes-jitsi-bosh record
```

Бот входит без камеры и микрофона. Для каждого участника создаются отдельные
`<participant_id>-audio.webm` (Opus) и `<participant_id>-video.webm` (VP8).
Файлы открываются при появлении участника; если микрофон или камера выключены,
соответствующий файл может остаться пустым. ID участника и его имя видны в
журнале `New participant`. Для остановки нажмите Ctrl-C. По умолчанию имя бота
`rust-recording-bot`; желательно, чтобы участники комнаты знали о записи.

Настройки в `.env.record`:

| Переменная | Значение |
| --- | --- |
| `JITSI_BOSH_URL` | HTTPS BOSH endpoint, здесь `https://meet.example.org/http-bind` |
| `JITSI_ROOM` | Человеческое имя комнаты, здесь `комната` |
| `JITSI_NICK` | Отображаемое имя бота |
| `RECORDINGS_DIR` | Каталог записи внутри контейнера, обычно `/data/recordings` |

Rust-сервис кодирует кириллическое имя комнаты в строчные percent-байты,
как это делает веб-клиент Jitsi. Без этого бот попадает в **другую** комнату.
Перед запуском в другой комнате поменяйте `JITSI_ROOM`.

## Что проверено

24 октября 2026 г. на `https://meet.example.org/комната` бот:

1. Подключился к BOSH и появился в браузерном списке участников.
2. Получил отдельные аудио и видео потоки участника `Бильбо`.
3. Записал ненулевые WebM-файлы (363 КБ аудио и 1,5 МБ видео в контрольном запуске).
4. Оба файла успешно прочитаны GStreamer `matroskademux`.

Это подтверждает короткую запись одного участника. Запись многих участников,
повторные входы, экранная демонстрация, длительная работа и полнота видео при
ограничениях пропускной способности Jitsi ещё не проверены. Запись по каждому
участнику сделана через шаблон `gst-meet`; дополнительные видеоисточники одного
участника в этот прототип не входят.

## Ограничения прототипа

- Базовый `gst-meet` считает API нестабильным. Патч BOSH локален и должен
  проверяться при обновлении upstream.
- Upstream сейчас пишет `Remote DTLS fingerprint (verification not implemented
  yet)`. Поэтому эту сборку следует считать проверочным прототипом, пока
  аутентификация медиаканала не будет исправлена и проверена.
- Каталог записей содержит реальные медиа участников. Храните его с
  ограниченным доступом. Политика хранения и согласия на запись ещё не
  реализована.
- Hermes, транскрибация и администратор конференции пока не подключены.

Для исходного WebSocket-теста остаётся [Dockerfile](Dockerfile) и команда
`hermes-jitsi-bridge run`. На `meet.example.org` используйте `Dockerfile.bosh`.

Источники: [gst-meet](https://github.com/jbg/gst-meet),
[Rust-форк с прежней реализацией BOSH](https://git.lain.faith/haskal/gst-meet),
[документация Jitsi по XMPP WebSocket](https://jitsi.github.io/handbook/docs/devops-guide/devops-guide-docker/#reverse-proxy-configuration).
