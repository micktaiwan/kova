# Multi-window & tab detach

## Objectif

Détacher un tab entier (avec tous ses splits) vers une nouvelle fenêtre, et pouvoir le rattacher à une fenêtre existante par drag.

## État actuel

### Ce qui est compatible

- `TerminalState` est déjà `Arc<RwLock<TerminalState>>` → partageable sans refacto.
- `Tab` est une struct indépendante (id, tree, focus, title) → facile à move entre fenêtres.
- `Pane` est owned dans `SplitTree::Leaf(Pane)` → pour un move de tab entier, pas besoin d'Arc-wrapper. Le move Rust transfère le PTY, le reader thread et tout le state d'un coup.

### Ce qui bloque

| Composant | Problème | Fichier |
|-----------|----------|---------|
| `AppDelegate` | Une seule `NSWindow` dans un `OnceCell` | `src/app.rs` |
| `KovaView` | Tabs, renderer, timer, config tous dans les ivars d'une seule view | `src/window.rs` |
| `Renderer` | Un seul, lié à un `CAMetalLayer` unique | `src/renderer/mod.rs` |
| `GlyphAtlas` | Textures GPU device-specific, non partageables | `src/renderer/glyph_atlas.rs` |
| Render timer | `NSTimer` qui capture un pointeur brut vers un seul `KovaViewIvars` | `src/window.rs` |

## Approche : move ownership (pas shared)

Le détach de tab entier permet une approche simple : **move, pas clone ni share**.

```
Fenêtre A                         Fenêtre B (nouvelle)
┌─────────────────┐               ┌─────────────────┐
│ tabs: [T1, T2]  │  ── move T2 → │ tabs: [T2]      │
│ renderer A      │               │ renderer B      │
│ atlas A         │               │ atlas B         │
│ timer A         │               │ timer B         │
└─────────────────┘               └─────────────────┘
```

Le PTY reader thread ne sait pas dans quelle fenêtre il vit — il écrit dans `Arc<RwLock<TerminalState>>` qui suit le move. Le render timer de la nouvelle fenêtre le lira naturellement.

## Décisions UX

- **Detach uniquement par drag souris** — pas de raccourci clavier (Cmd+Shift+D déjà pris pour le swap de split).
- **Pas de detach si un seul tab** — ça n'a pas de sens, le tab est déjà seul dans sa fenêtre.
- **Position de la nouvelle fenêtre** — là où la souris drop le tab. Permet de placer sur un deuxième écran naturellement.
- **Drop sur une tab bar existante = reattach** — si le tab est droppé sur la tab bar d'une autre fenêtre, il est rattaché à cette fenêtre au lieu de créer une 3e fenêtre.
- **Drop ailleurs = nouvelle fenêtre** — si le tab est droppé hors d'une tab bar, une nouvelle fenêtre est créée à la position du curseur.
- **Session restore multi-écran** — les positions de toutes les fenêtres sont mémorisées. Un Cmd+Q + réouverture restaure chaque fenêtre avec ses tabs/splits à la bonne position, y compris sur le bon écran.
- **Config par fenêtre (futur)** — pour l'instant config globale, mais l'architecture doit permettre à terme des couleurs/thèmes différents par fenêtre.

## Étapes d'implémentation

### 1. Refacto multi-window (`AppDelegate`)

**Fichier** : `src/app.rs`

Passer de `OnceCell<Retained<NSWindow>>` à une gestion multi-fenêtres :

```rust
// Avant
window: OnceCell<Retained<NSWindow>>

// Après — par exemple un Vec global ou un registre
windows: RefCell<Vec<Retained<NSWindow>>>
```

Extraire la logique de création de fenêtre (`NSWindow` + `KovaView` + Metal setup + render timer) dans une fonction réutilisable `create_window(tabs: Vec<Tab>) -> Retained<NSWindow>`.

Gérer la fermeture : quand une fenêtre n'a plus de tabs, elle se ferme. Quand plus aucune fenêtre, `app.terminate`.

### 2. Factoriser la création de `KovaView`

**Fichier** : `src/window.rs`

Aujourd'hui le setup Metal, la création du renderer et le timer sont dans `viewDidMoveToWindow` ou l'init. Extraire en fonctions appelables pour une nouvelle fenêtre :

- `setup_metal(layer) -> Renderer`
- `start_render_timer(ivars) -> Retained<NSTimer>`

Le `Config` peut être partagé (clone ou `Arc<Config>`) entre fenêtres — pas de raison d'en avoir un par fenêtre.

### 3. Detach : move d'un tab

Quand l'utilisateur drag un tab hors de la tab bar :

```rust
fn detach_tab(source_view: &KovaView, tab_index: usize) {
    // 1. Retirer le tab du Vec<Tab> source
    let tab = source_view.tabs.borrow_mut().remove(tab_index);

    // 2. Créer une nouvelle fenêtre à la position du curseur
    create_window_at(vec![tab], mouse_position);

    // 3. Si la source n'a plus de tabs, fermer la fenêtre source
    // (ne devrait pas arriver car detach bloqué si un seul tab)
}
```

L'atlas de la nouvelle fenêtre se construit progressivement (rasterisation à la demande, comme aujourd'hui). Premier frame un peu plus lent, imperceptible.

### 4. Reattach : move inverse

Le drag est un geste unifié — c'est la destination du drop qui détermine le résultat :

- **Drop sur tab bar d'une autre fenêtre** → reattach (move dans le `Vec<Tab>` cible)
- **Drop ailleurs** → nouvelle fenêtre à la position du curseur

```rust
fn reattach_tab(target_view: &KovaView, tab: Tab, insert_index: usize) {
    // 1. Insérer le tab à la position du drop dans la tab bar
    target_view.tabs.borrow_mut().insert(insert_index, tab);

    // 2. Activer le tab importé
    target_view.active_tab.set(insert_index);

    // 3. Fermer la fenêtre source si elle n'a plus de tabs
}
```

Le drag cross-window nécessite `NSDraggingSource`/`NSDraggingDestination` d'AppKit, ou un mécanisme custom avec détection de hit-test sur les tab bars de toutes les fenêtres au moment du drop.

### 5. Session save/restore

**Fichier** : `src/session.rs`

Adapter pour sauvegarder N fenêtres au lieu d'une :

```rust
struct SessionData {
    windows: Vec<WindowSession>,
}

struct WindowSession {
    tabs: Vec<TabSession>,
    frame: NSRect,  // position/taille de la fenêtre
    screen: Option<String>,  // identifiant écran pour restore multi-monitor
}
```

Chaque fenêtre sauvegarde sa position et son écran. Au restore, si l'écran n'est plus disponible, fallback sur l'écran principal avec la même taille.

## Compromis

| Aspect | Choix | Justification |
|--------|-------|---------------|
| Atlas GPU | Dupliqué par fenêtre | ~2-4 Mo par fenêtre, simple et propre. Partager entre devices Metal est complexe pour un gain négligeable. |
| Config | Globale pour l'instant | Une seule config partagée. L'architecture doit permettre à terme une config/thème par fenêtre. |
| PTY ownership | Move avec le tab | Pas de refacto Arc. Le reader thread continue d'écrire dans le même `Arc<RwLock<TerminalState>>`. |
| Drag cross-window | Via AppKit dragging | Plus natif que du custom hit-testing. |

## Risques

- **Thread safety** : le render timer de la nouvelle fenêtre accède aux mêmes `Arc<RwLock<TerminalState>>` que le PTY reader thread. C'est déjà le cas aujourd'hui, pas de changement.
- **PaneId unicité** : le compteur global `PaneId` garantit l'unicité cross-fenêtres. OK.
- **TabId unicité** : idem, compteur global. OK.
- **Focus cross-window** : `NSWindow.keyWindow` gère ça nativement. Le pane focusé est par-tab, donc pas d'ambiguïté.
- **Fermeture app** : il faut `app.terminate` uniquement quand toutes les fenêtres sont fermées, pas juste la dernière. Utiliser `applicationShouldTerminateAfterLastWindowClosed` ou un compteur.

## Ordre de priorité

1. Multi-window — refacto `AppDelegate` + factorisation création fenêtre (prérequis à tout)
2. Drag detach — tab drag hors tab bar → nouvelle fenêtre à la position du drop
3. Drop reattach — drop sur tab bar d'une autre fenêtre → rattachement
4. Session save/restore multi-window — positions + écrans mémorisés
