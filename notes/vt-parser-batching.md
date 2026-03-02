# VT Parser Batching — Réduction de la contention write lock

## Problème

Le PTY reader thread appelle `parser.advance()` sur un chunk de 4 Ko. Pendant toute la durée du parsing, le write lock sur `TerminalState` est tenu (acquisition lazy au premier `self.term()`, release après `advance()`).

`parking_lot::RwLock` donne priorité aux writers. Quand un pane en background reçoit beaucoup de données (build, logs…), le renderer sur le main thread est bloqué en attente du read lock — même pour les panes qui ne sont pas en train d'écrire.

Résultat : lag visible au switch de tab ou pendant le rendu quand un build tourne en background.

## Solution : Op-buffer + flush batch

### Principe

Au lieu que les callbacks VTE (`print`, `execute`, `csi_dispatch`…) mutent `TerminalState` directement via `self.term()`, ils accumulent des opérations dans un `Vec<TermOp>` local. Après `parser.advance()`, on prend le write lock **une seule fois**, on replay les ops, puis on release.

Le parsing (décoder les séquences VT) se fait **sans lock**. Seul le replay (mutations sur la grille) nécessite le lock. Le replay est beaucoup plus rapide car il n'y a plus de logique de parsing — juste des mutations directes.

### Enum `TermOp`

```rust
enum TermOp {
    /// Texte à afficher (grapheme clusters déjà découpés)
    Print(String),
    /// Control characters
    Backspace,
    Tab,
    Newline,
    CarriageReturn,
    Bell,
    /// Cursor movement
    CursorUp(u16),
    CursorDown(u16),
    CursorForward(u16),
    CursorBackward(u16),
    SetCursorPos(u16, u16),
    /// Cursor Horizontal Absolute — besoin du cursor_y courant
    SetCursorCol(u16),
    /// Vertical Position Absolute — besoin du cursor_x courant
    SetCursorRow(u16),
    SaveCursor,
    RestoreCursor,
    /// Erasing
    EraseInDisplay(u16),
    EraseInLine(u16),
    EraseChars(u16),
    /// Lines
    InsertLines(u16),
    DeleteLines(u16),
    DeleteChars(u16),
    InsertChars(u16),
    /// Scroll
    ScrollUp(u16),
    ScrollDown(u16),
    SetScrollRegion(u16, Option<u16>), // top, bottom (None = use rows)
    /// Modes
    SetDecMode(u16, bool),  // mode number, on/off
    SetMode(u16, bool),
    /// SGR
    SetSgr(Vec<u16>),
    /// Cursor shape
    SetCursorShape(u16),
    /// Screen
    EnterAltScreen,
    LeaveAltScreen,
    ReverseIndex,
    FullReset,
    /// Metadata (no state read needed)
    SetTitle(String),
    SetCwd(String, Option<String>), // path, git_branch
    SetLastCommand(String),
    /// Responses — nécessitent lecture de l'état puis écriture PTY
    /// Traitées spécialement pendant le replay.
    CursorPositionReport,
    DeviceAttributes,
    ReportPrivateMode(u16),
    KittyKeyboardQuery,
}
```

### Shadow state

Certaines ops ont besoin de **lire** l'état courant du terminal pour construire l'op. Il y a deux approches :

**Option A — Ops de haut niveau (recommandée)** : encoder l'intention plutôt que le résultat. Par exemple `SetCursorCol(col)` au lieu de `SetCursorPos(cursor_y, col)`. Le replay résout la dépendance au moment de l'application. C'est ce que fait l'enum ci-dessus.

**Option B — Shadow state local** : tracker `cursor_x`, `cursor_y`, `rows`, `cols` localement dans le handler pour construire les ops complètes. Plus complexe et fragile (le shadow state peut diverger).

### Callbacks qui lisent l'état — analyse

| Callback | Ce qui est lu | Solution Option A |
|----------|--------------|-------------------|
| CSI `G` (CHA) | `cursor_y` | `SetCursorCol(col)` — le replay fait `set_cursor_pos(cursor_y, col)` |
| CSI `d` (VPA) | `cursor_x` | `SetCursorRow(row)` — le replay fait `set_cursor_pos(row, cursor_x)` |
| CSI `r` (DECSTBM) | `rows` | `SetScrollRegion(top, None)` — le replay utilise `rows` comme default |
| CSI `n` (CPR) | `cursor_y/x` | `CursorPositionReport` — le replay lit la position et écrit au PTY |
| CSI `p` (DECRPM) | mode flags | `ReportPrivateMode(mode)` — le replay lit le flag et écrit au PTY |
| ESC `c` (RIS) | dims, colors | `FullReset` — le replay lit les valeurs et reconstruit |

**Conclusion** : l'option A couvre 100% des cas. Aucun shadow state nécessaire.

### OSC 7 — cas spécial

OSC 7 (set CWD) fait un I/O filesystem (`resolve_git_branch`) qui est déjà géré en relâchant le lock. Avec le batching, on fait le I/O pendant le parsing (pas de lock tenu) et on push `SetCwd(path, git_branch)` dans le buffer.

### Réponses PTY (CPR, DA1, DECRPM, Kitty query)

Ces callbacks lisent l'état *et* écrivent au PTY. Avec le batching :
1. On push un op marker (`CursorPositionReport`, etc.)
2. Pendant le replay, au moment de traiter ce marker, on lit l'état courant (déjà mis à jour par les ops précédentes) et on écrit au PTY

Cela introduit un délai minime sur la réponse (le temps du replay), mais c'est négligeable car le replay est rapide et ces réponses ne sont pas time-critical.

### Implémentation

1. Ajouter `ops: Vec<TermOp>` dans `VteHandler`
2. Tous les callbacks push dans `self.ops` au lieu d'appeler `self.term()`
3. `flush_print_buf()` devient un push de `Print(buf)` dans ops
4. Nouvelle méthode `fn apply_ops(&mut self)` :
   - Prend le write lock une fois
   - Itère sur `self.ops.drain(..)`
   - Applique chaque op sur `TerminalState`
   - Release le lock
5. Dans `pty.rs`, après `parser.advance()` : appeler `handler.apply_ops()` au lieu de `handler.release_guard()`

### Changements dans `pty.rs`

```rust
// Avant
parser.advance(&mut handler, &buf[..n]);
handler.release_guard();

// Après
parser.advance(&mut handler, &buf[..n]);
handler.apply_ops();
```

### Performance attendue

- **Durée du write lock** : passe de "temps de parsing VT d'un chunk 4 Ko" à "temps de replay d'un Vec d'ops déjà parsées"
- Le parsing (la partie lente : décoder les séquences, split graphemes, etc.) se fait sans lock
- Le replay est une simple boucle de match + mutations directes
- Estimation : réduction de 5-10× du temps sous write lock pour un chunk typique

### Risques

- **Taille de l'enum `TermOp`** : ~40 variantes. C'est le gros du travail d'implémentation mais c'est mécanique.
- **Allocation du Vec** : un `Vec::with_capacity(256)` réutilisé entre les reads suffit (drain, pas de réalloc).
- **Correction** : le replay doit reproduire exactement le même comportement qu'avant. Tests manuels nécessaires avec `htop`, `vim`, `git log --oneline`, builds cargo, etc.
