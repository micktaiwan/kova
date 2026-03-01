# Multi-window & tab detach

## Objectif

Détacher un tab entier (avec tous ses splits) vers une nouvelle fenêtre, et pouvoir le rattacher à une fenêtre existante par drag.

## État actuel

### Implémenté (étapes 1-3)

- **Multi-window** : `AppDelegate` gère un `Vec<Retained<NSWindow>>` au lieu d'un `OnceCell`.
- **Timer global unique** : un seul `NSTimer` dans `AppDelegate` itère toutes les fenêtres et appelle `view.tick()`. Chaque `tick()` retourne `bool` — `false` = fenêtre morte (plus de tabs), le timer la ferme et la retire du Vec.
- **`create_window(mtm, config, tabs, active_tab)`** : fonction unique pour créer une fenêtre avec des tabs donnés. Plus de session restore implicite dans `setup_metal`.
- **Session restore multi-window** : `AppDelegate::did_finish_launching` charge la session et crée N fenêtres avec leurs tabs et positions (`setFrame_display`).
- **Session save multi-window** : format v2 avec `Vec<WindowSession>` (backward compat v1). Chaque fenêtre sauvegarde tabs + active_tab + frame (x, y, w, h).
- **Cmd+N** : nouvelle fenêtre vide (tab + shell frais). Accède à l'AppDelegate via `msg_send![self]` pour enregistrer la fenêtre.
- **Autosave name unique** : chaque fenêtre a un `KovaWindow-{id}` via `AtomicU32`, évite les collisions NSUserDefaults.
- **`kova_view()`** : pub dans `app.rs`, réutilisable.
- **Cmd+Shift+T** : detach le tab actif vers une nouvelle fenêtre (no-op si un seul tab). Nouvelle fenêtre décalée +20x/-20y (cascade).
- **Cmd+Q = fermer fenêtre active** : flag `closing` vérifié par `tick()`, confirmation si processes en cours. L'app termine quand la dernière fenêtre est fermée.
- **Dealloc différé** : les fenêtres fermées sont déplacées dans `pending_close` (Vec) avec `orderOut`, puis dealloc'ées au tick suivant pour éviter les segfaults AppKit.

### Ce qui reste à faire

- Drag detach avec preview fantôme
- Drop reattach sur tab bar d'une autre fenêtre

## Architecture

```
AppDelegate (src/app.rs)
├── windows: RefCell<Vec<Retained<NSWindow>>>
├── pending_close: RefCell<Vec<Retained<NSWindow>>>  ← dealloc différé d'un tick
├── closed_sessions: RefCell<Vec<WindowSession>>     ← sessions des fenêtres fermées
├── config: OnceCell<Config>
├── start_global_timer(fps)     ← un seul NSTimer pour toutes les fenêtres
├── app_delegate(mtm)           ← helper pour accéder à l'AppDelegate
├── create_new_window(mtm)      ← Cmd+N
├── detach_tab_to_new_window()  ← Cmd+Shift+T
└── did_finish_launching()      ← session restore → N fenêtres

KovaView (src/window.rs)
├── setup_metal(mtm, config, tabs, active_tab)  ← plus de session::load ici
├── tick() -> bool              ← appelé par le timer global, false = à fermer
├── do_close_window()           ← Cmd+Q, set closing flag
├── do_detach_tab()             ← Cmd+Shift+T, retire tab et crée fenêtre
├── append_session_data(&mut Vec<WindowSession>) ← collecte pour save
├── confirm_running_processes() ← alerte partagée (should_terminate + close)
└── ivars: closing, git_poll_interval, git_poll_counter, last_title, ...

Session (src/session.rs)
├── Session { version: 2, windows: Vec<WindowSession> }
├── WindowSession { tabs, active_tab, frame }
├── WindowSession::from_tabs()  ← helper pour save
├── save(&[WindowSession])      ← un seul fichier pour toutes les fenêtres
├── load() + restore_session()  ← backward compat v1 → v2
└── RestoredWindow { tabs, active_tab, frame }
```

## Décisions UX

- **Cmd+N = nouvelle fenêtre vide** — ouvre une nouvelle fenêtre avec un tab + shell frais.
- **Cmd+Shift+T = detach tab actif** — raccourci clavier pour détacher le tab actif vers une nouvelle fenêtre. Sert de premier pas avant le drag.
- **Detach par drag souris (phase 2)** — drag un tab hors de la tab bar pour le détacher. Preview fantôme du tab pendant le drag.
- **Pas de detach si un seul tab** — ça n'a pas de sens, le tab est déjà seul dans sa fenêtre.
- **Position de la nouvelle fenêtre** — pour le drag : là où la souris drop le tab. Pour Cmd+Shift+T/Cmd+N : décalée par rapport à la fenêtre active.
- **Drop sur une tab bar existante = reattach** — si le tab est droppé sur la tab bar d'une autre fenêtre, il est rattaché à cette fenêtre au lieu de créer une 3e fenêtre.
- **Drop ailleurs = nouvelle fenêtre** — si le tab est droppé hors d'une tab bar, une nouvelle fenêtre est créée à la position du curseur.
- **Cmd+Q ferme la fenêtre active** — ferme tous les tabs de la fenêtre courante, pas toute l'app. L'app se termine quand plus aucune fenêtre n'est ouverte. Session sauvegardée.
- **Cmd+Option+Q = kill fenêtre** — ferme la fenêtre sans sauvegarder sa session (la fenêtre ne sera pas restaurée au prochain lancement).
- **Session restore multi-écran** — les positions de toutes les fenêtres sont mémorisées. Un quit + réouverture restaure chaque fenêtre avec ses tabs/splits à la bonne position, y compris sur le bon écran.
- **Config par fenêtre (futur)** — pour l'instant config globale, mais l'architecture doit permettre à terme des couleurs/thèmes différents par fenêtre.

## Étapes d'implémentation

### ~~1. Refacto multi-window~~ ✅

- `AppDelegate` : `OnceCell<NSWindow>` → `RefCell<Vec<Retained<NSWindow>>>`
- Timer global unique dans `AppDelegate` qui appelle `tick()` sur chaque `KovaView`
- `tick()` retourne `bool` ; le timer ferme les fenêtres mortes et les retire du Vec
- `create_window(mtm, config, tabs, active_tab)` — signature unifiée
- `setup_metal` ne fait plus de session restore — il prend des tabs explicites
- Session restore dans `did_finish_launching` avec positions de fenêtres
- `applicationShouldTerminateAfterLastWindowClosed` → `true`
- `should_terminate` collecte les processes de toutes les fenêtres
- `will_terminate` sauvegarde toutes les fenêtres dans un seul fichier session

### ~~2. Cmd+N~~ ✅

- `Cmd+N` dans `performKeyEquivalent` → `crate::app::create_new_window(mtm)`
- `create_new_window` accède à l'AppDelegate via `msg_send![&*delegate, self]`
- Crée un tab frais, appelle `create_window`, enregistre dans le Vec
- Autosave name unique par fenêtre (`KovaWindow-{AtomicU32}`)

### ~~Session v2~~ ✅

- `Session { version: 2, windows: Vec<WindowSession> }`
- `WindowSession { tabs, active_tab, frame: Option<(f64,f64,f64,f64)> }`
- Backward compat v1 : `SessionV1` deserialize → migration automatique
- `WindowSession::from_tabs()` helper pour construire depuis live data
- `save(&[WindowSession])` — un fichier unique
- `restore_session()` → `Vec<RestoredWindow>`

### ~~3. Cmd+Shift+T — detach tab actif~~ ✅

Retirer le tab actif du Vec source, créer une nouvelle fenêtre avec ce tab. Bloquer si un seul tab. Inclut aussi Cmd+Q = fermer la fenêtre active (pas l'app entière), avec dealloc différé d'un tick via `pending_close` pour éviter les segfaults AppKit.

### 4. Drag detach avec preview fantôme

Tab drag hors tab bar → nouvelle fenêtre à la position du drop. Nécessite un seuil de distance pour distinguer reorder de detach.

### 5. Drop reattach

Drop sur tab bar d'une autre fenêtre → rattachement. Via `NSDraggingSource`/`NSDraggingDestination` d'AppKit ou hit-test custom sur les tab bars de toutes les fenêtres.

## Compromis

| Aspect | Choix | Justification |
|--------|-------|---------------|
| Atlas GPU | Dupliqué par fenêtre | ~2-4 Mo par fenêtre, simple et propre. Partager entre devices Metal est complexe pour un gain négligeable. |
| Config | Globale pour l'instant | Une seule config clonée par fenêtre. L'architecture doit permettre à terme une config/thème par fenêtre. |
| PTY ownership | Move avec le tab | Pas de refacto Arc. Le reader thread continue d'écrire dans le même `Arc<RwLock<TerminalState>>`. |
| Timer | Global unique | Un seul `NSTimer` dans `AppDelegate` itère toutes les fenêtres. Évite N timers idle. |
| Session file | Un seul fichier multi-window | `~/.config/kova/session.json` avec `Vec<WindowSession>`. Backward compat v1. |
| Autosave names | `KovaWindow-{id}` | Compteur `AtomicU32` global, unique par session. |
| Dealloc fenêtre | Différé d'un tick | `pending_close` + `orderOut` immédiat. Drop au tick suivant pour éviter segfault AppKit (callbacks sur vue dealloc'ée). |
| Session save | Eager + terminate | `closed_sessions` collecte au moment du close. `will_terminate` merge closed + live + pending. |

## Risques

- **Thread safety** : le timer global accède aux `KovaView` via cast de `contentView` — même pattern que `kova_view()`, pas de nouveau risque.
- **PaneId/TabId unicité** : compteurs globaux, OK cross-fenêtres.
- **Focus cross-window** : `NSWindow.keyWindow` gère ça nativement. Le pane focusé est par-tab, donc pas d'ambiguïté.
- **Fermeture app** : `applicationShouldTerminateAfterLastWindowClosed` = `true` + le timer appelle `app.terminate` quand le Vec est vide.
- **Double borrow** : `tick()` ne ferme jamais de fenêtre directement — il retourne `false` et le timer gère la fermeture après avoir relâché le borrow.
- **Segfault dealloc AppKit** : dropper une `Retained<NSWindow>` dans le même tick que `close()`/`orderOut()` cause un segfault (AppKit garde des refs internes à la vue dans la run loop). Résolu par `pending_close` : dealloc différé au tick suivant.
- **Session perdue au close** : les fenêtres fermées avant le quit perdaient leurs données. Résolu par `closed_sessions` qui collecte au moment du close.

## Ordre de priorité

1. ~~Multi-window — refacto `AppDelegate` + factorisation création fenêtre + timer global unique~~ ✅
2. ~~Cmd+N — nouvelle fenêtre vide~~ ✅
3. ~~Cmd+Shift+T — detach tab actif vers nouvelle fenêtre (valide le move de tab sans drag)~~ ✅
4. Drag detach avec preview fantôme — tab drag hors tab bar → nouvelle fenêtre à la position du drop
5. Drop reattach — drop sur tab bar d'une autre fenêtre → rattachement
