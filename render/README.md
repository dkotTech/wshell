# render-rs

Headless HTML-рендер для wshell: рендерит веб-страницы без дисплея и стримит
их как видео (PNG-кадры) по WebSocket. Управляется удалённо по HTTP API —
клавиатура, мышь, колесо.

Каждый рендер загружает HTML с указанного endpoint'а (например, UI приложения
wshell) — рендеров можно запустить несколько.

Это отдельный процесс: запускается независимо от `shelld`. Крейт входит в
workspace wshell, но не в `default-members` — обычный `cargo build` его не
собирает (Servo тяжёлый), только явно через `-p render-rs`.

## Движки

Движок выбирается на этапе сборки cargo-фичами (взаимоисключающие):

```bash
# WPE WebKit (по умолчанию) — C FFI, бинарь ~5 MB
# зависимости: sudo pacman -S wpewebkit  (тянет libwpe, wpebackend-fdo)
cargo build --release -p render-rs

# Servo — чистый Rust, без системных зависимостей, бинарь ~80 MB
cargo build --release -p render-rs --no-default-features --features servo
```

|                       | webkit (WPE)     | servo              |
|-----------------------|------------------|--------------------|
| Headless              | ✅ (FDO SHM)     | ✅ (software ctx)  |
| CSS/JS совместимость  | высокая          | хорошая            |
| Зависимости           | пакет wpewebkit  | нет                |
| Бинарь                | ~5 MB            | ~80 MB             |

## Запуск

```bash
./target/release/render-rs                      # лаунчер-плагин wshell, 256×144, API на 127.0.0.1:8090
./target/release/render-rs -c render.toml       # конфиг: см. render.example.toml
./target/release/render-rs --url http://localhost:5173/ --width 640 --height 480   # произвольная страница
```

По умолчанию рендер открывает **лаунчер wshell** — плагин `org.wshell.launcher`
(`plugins/launcher`): берёт одноразовую ссылку на его UI у `shelld` через
дополнительный управляющий сокет `render.sock`, на котором разрешено только это
(`[control.sockets.render]` в конфиге Shell, `allow = ["url:org.wshell.launcher"]`).
Если `shelld` ещё не запущен — повторяет раз в 2 с.
Из лаунчера приложения открываются стрелками и Enter, клавиша `AppSwitch`
возвращает в лаунчер (каждый раз с новой ссылкой). Приложение, которое ушло
с экрана, Shell приостанавливает сам.

`--url` вместо лаунчера поднимает отдельный рендер на каждый адрес. Дальше
рендеры можно создавать/удалять на лету через API.

### Конфиг

Все поля необязательны, пример с умолчаниями — [`render.example.toml`](render.example.toml):

- `[api]` — `listen` (только loopback по умолчанию) и `token`;
- `[shell]` — id плагина-лаунчера (`""` — без домашнего экрана) и путь к `render.sock`;
- `[screen]` — размер и fps рендера по умолчанию (экран устройства);
- `[keys]` — клавиши по именам `KeyboardEvent.key`: `map` переименовывает
  (`GoBack = "Escape"`), `actions` — клавиши самого рендера
  (`AppSwitch = "launcher"`), страница их не получает.

### Доступ

API действует в приложениях от имени пользователя, поэтому:

- слушает `127.0.0.1` (`[api] listen`);
- каждый запрос — с токеном: `Authorization: Bearer <token>`, `?token=<token>`
  (заодно ставит cookie — так открывается веб-интерфейс в браузере) или cookie;
- токен — из конфига, `--token` / `RENDER_TOKEN` или случайный на каждый запуск;
  адрес с токеном печатается при старте;
- без CORS: чужие страницы в браузере к API не обращаются.

## Терминал (TUI)

```bash
render-rs -c render.toml tui              # свой рендер размером с окно терминала, в нём лаунчер
render-rs -c render.toml tui --render 1   # подключиться к рендеру 1 (экран устройства), 1:1
```

Показывает рендер прямо в терминале и управляет им — в том числе по SSH на
устройстве. Нужен терминал с **графическим протоколом kitty**: kitty,
WezTerm, Ghostty (проверяется при запуске).

- **Свой рендер** (по умолчанию): размером с окно терминала в пикселях (минус
  строка статуса), страница верстается под экран компьютера; окно меняет
  размер — рендер тоже. Рендер временный (`ephemeral`): удаляется, когда
  уходит последний зритель, в том числе при обрыве SSH.
- **`--render N`** — смотреть существующий рендер, например экран устройства,
  пиксель в пиксель. Его TUI не удаляет.
- Картинка: PNG из WebSocket рендера как есть, одно изображение с
  фиксированным id — терминал заменяет его на месте. Неизменившиеся кадры не
  отправляются.
- Клавиатура: **протокол клавиатуры kitty** — нажатие, повтор и отпускание,
  то есть удержание клавиш работает как на устройстве. Без него — целые нажатия.
- Мышь: SGR в пикселях (`?1016h`) — это и есть пиксели рендера.
- `[tui.keys]` переименовывает клавиши терминала (`F12 = "AppSwitch"` —
  у терминала нет кнопки переключения приложений), дальше действует `[keys]`.
- `Ctrl+Q` / `Ctrl+C` — выход; сочетания с Ctrl/Alt в страницу не уходят.
- TUI — клиент API: нужен токен рендера (`[api] token`, `--token`, `RENDER_TOKEN`).

## API

| Метод / путь                     | Что делает |
|----------------------------------|------------|
| `GET /`                          | менеджер рендеров (web UI) |
| `GET /view/{id}`                 | интерактивный viewer: canvas + проброс мыши/клавиатуры; клавиши переименовывает `[web.keys]` (F12 → AppSwitch), кнопка «⌂ apps» — то же самое |
| `GET /api/renders`               | список рендеров |
| `POST /api/renders`              | создать: `{"url":"http://...","width":256,"height":144,"fps":30}` (размеры и fps — необязательны, по умолчанию `[screen]`); `"launcher":true` вместо `url` — лаунчер wshell; `"ephemeral":true` — удалить, когда уйдёт последний WebSocket-зритель |
| `POST /api/renders/{id}/resize`  | новый размер: `{"width":1200,"height":780}` — страница перестраивается |
| `DELETE /api/renders/{id}`       | остановить рендер |
| `POST /api/renders/{id}/input`   | событие ввода (см. ниже) |
| `POST /api/renders/{id}/navigate`| перейти на другой url: `{"url":"..."}` |
| `POST /api/renders/{id}/launcher`| вернуться в лаунчер wshell (то же, что клавиша `AppSwitch`) |
| `GET /api/renders/{id}/ws`       | WebSocket: text-метаданные `{w,h,url}`, затем бинарные PNG-кадры; входящие text-сообщения — те же input-события |
| `GET /api/renders/{id}/frame`    | последний кадр как PNG (скриншот) |
| `GET /api/renders/{id}/stream`   | multipart-поток PNG (fallback без WebSocket) |

### Raw-подписка для дисплеев (MCU)

Тот же WS-эндпоинт с query-параметрами — сервер сам конвертирует кадры:

```
GET /api/renders/{id}/ws?format=rgb565&w=256&h=144&fps=10&token=<token>
```

- `format`: `png` (по умолчанию) | `rgb565` | `rgb888`
- `w`,`h`: серверный даунскейл (Nearest); без них — родной размер рендера
- `fps`: персональный темп подписчика (1–60), независим от других клиентов

Бинарное сообщение: `"FR" | fmt u8 | flags u8 | w u16 le | h u16 le | seq u32 le | pixels`.

### События ввода

`key` использует значения JS `KeyboardEvent.key` ("a", "Enter", "ArrowUp", " ",
"GoBack", "AppSwitch"); перед отправкой в страницу к нему применяется `[keys]`.

```jsonc
{"type":"key","key":"Enter"}                          // state: press (по умолч.) | down | up
{"type":"mouse_move","x":300,"y":180}
{"type":"mouse_button","button":"left","x":300,"y":180} // state: click (по умолч.) | down | up
{"type":"wheel","x":320,"y":240,"dy":40}
```

Пример — клик и набор текста:

```bash
auth="Authorization: Bearer $RENDER_TOKEN"
curl -X POST localhost:8090/api/renders/1/input -H "$auth" \
  -H 'Content-Type: application/json' \
  -d '{"type":"mouse_button","x":120,"y":36}'
curl -X POST localhost:8090/api/renders/1/input -H "$auth" \
  -H 'Content-Type: application/json' -d '{"type":"key","key":"1"}'
```

## Как устроено

```
main thread                    per render                 tokio thread
┌─────────────────┐   spec    ┌──────────────────────┐   ┌─────────────────┐
│ engine loop      │◄──mpsc───│                      │   │ axum API :8090  │
│  webkit: GLib    │          │ pixels → sync(1) →   │   │  /api/renders   │
│  servo: spin     │──frames─►│ encoder thread (PNG) │──►│  /ws /frame ... │
└─────────────────┘           │  → broadcast(4)      │   └─────────────────┘
                              └──────────────────────┘
```

- Движок владеет главным потоком (GLib main loop / servo event loop);
  команды из API приходят по каналу.
- Кодирование PNG — в отдельном потоке на рендер; занятый кодировщик
  просто пропускает кадр (sync_channel(1)), рендер не блокируется.
- Последний кадр кешируется: новый WebSocket-клиент получает картинку
  сразу, не дожидаясь перерисовки страницы.

## Известные особенности

- WPE: выпадающий список нативного `<select>` не отрисовывается
  (попап рисует embedder). Значение можно менять с клавиатуры
  (фокус + стрелки) или через JS.
- Servo: Enter в текстовом поле не отправляет форму, если кнопка отправки —
  `<button type=submit>` (servo ищет только `<input type=submit>`), а полей
  ввода в форме больше одного. WebKit отправляет по стандарту.
- Servo: частичная поддержка CSS grid — верстка может слегка отличаться;
  `requestAnimationFrame` без vsync не тикает, используйте `setInterval`.
- libEGL warnings при старте безвредны — рендер идёт в софте.
