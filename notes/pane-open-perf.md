# Perf d'ouverture d'un pane

## Le constat central : deux latences distinctes

« Ouvrir un pane » mélange **deux latences** qui ne se règlent pas avec les mêmes fixes :

1. **Time-to-rectangle** — le pane se dessine à l'écran. Retardée par du travail
   **sur le main thread AppKit** : `fork`/`exec` du shell, reflow synchrone des
   panes voisins (`resize_all_panes`), et jusqu'à une frame d'attente avant le
   prochain tick `NSTimer`.
2. **Time-to-prompt** — le shell devient utilisable (premier prompt). Dominée par
   le **login shell qui source `/etc/zprofile` + `~/.zshrc`** (oh-my-zsh, plugins)
   dans le process **enfant**. Tourne **déjà hors du thread de Kova** : aucune
   optimisation main-thread ou rendu ne la touche. Seul levier côté Kova : un pool
   de shells pré-chauffés.

Nuance vérifiée : sur macOS `fork` coûte en O(nombre d'entrées vm_map), **pas** en
O(octets mappés) — l'atlas de glyphes / les MTLBuffers ne le gonflent pas. Le
blocage du `fork` sur le main thread est de l'ordre d'une frame ou deux. La valeur
de sortir le spawn du main thread est le **découplage** (le pane apparaît avant la
fin du `fork`), pas de récupérer le temps du `fork`.

## Instrumentation (en place)

Trois jalons logués sous le préfixe `PANE-OPEN`, relatifs à une référence unique
`entry` = début de `Pane::spawn` (le travail pré-spawn dans les handlers de split
est du calcul flottant sub-µs, replié dedans). Implémentée par `PaneOpenTimer`
(`src/pane.rs`), partagée immuablement avec le thread de lecture PTY.

| Jalon | Sens | Posé dans |
|---|---|---|
| `tree-inserted` | spawn (fork/exec) + insertion dans l'arbre | `do_split` / `do_split_root` / `ipc_split` après `drop(tabs)`, avant `resize_all_panes` (`src/window.rs`) |
| `first-paint` | pane soumis au renderer = visible → **time-to-rectangle** | boucle de collecte `pane_data` dans `tick()` (`src/window.rs`) |
| `shell-ready` | premier octet du shell = premier prompt → **time-to-prompt** | thread de lecture, à la bascule `shell_ready` (`src/terminal/pty.rs`) |

Lecture :

```bash
grep PANE-OPEN ~/Library/Logs/Kova/kova.log
# PANE-OPEN id=7 tree-inserted +2.1ms
# PANE-OPEN id=7 first-paint  +5.4ms (time-to-rectangle)
# PANE-OPEN id=7 shell-ready  +212.0ms (time-to-prompt)
```

- Un split interactif émet **3 lignes** (`tree-inserted` + `first-paint` + `shell-ready`).
- Un pane de **restore** émet **2 lignes** (pas de `tree-inserted` : les handlers de
  restore n'appellent pas `mark_inserted`) → l'absence de `tree-inserted` distingue
  un restore d'un split.
- Chaque jalon ne loggue qu'une fois (garde atomique).
- `entry` ≈ keystroke à sub-µs près ; si on veut un jour le délai keystroke→spawn
  exact, capturer un `Instant` en tête des handlers de split (chemin main-thread,
  pas de race).

## Plan priorisé (issu de l'analyse multi-agents, findings vérifiés)

Mesurer **avant** d'optimiser : sans le découpage rectangle vs prompt, on optimise
la mauvaise moitié. C'est l'objet de l'instrumentation ci-dessus.

### Pour « le pane apparaît instantanément »

1. **Insérer un placeholder `shell_ready=false` AVANT de spawner, puis spawner le
   PTY hors du main thread et swapper** (impact élevé / complexité moyenne). Le
   meilleur ratio. Aujourd'hui les 3 entrées appellent `Pane::spawn` (fork/exec
   bloquant) **puis** insèrent → le rectangle ne peut pas se dessiner avant la fin
   du spawn. Détail : ne pas réutiliser `Pane::placeholder` tel quel, il met
   `shell_ready=true` (masquerait l'overlay de chargement). `Pty` est `Send`.
3. **Forcer un rendu synchrone après un split + drainer l'IPC avant le tick**
   (quick win). Il n'y a **aucun chemin `setNeedsDisplay`** : le rendu ne dépend
   que du `NSTimer`. Un `self.tick()` en fin de `do_split`/`do_split_root` (après
   `drop(tabs)`) supprime le plancher ~16,67 ms. Pour l'IPC, le drain (`app.rs`)
   tourne **après** `view.tick()` → un `ipc_split` rend une frame plus tard ; le
   remonter avant la boucle de tick.
4. **Découpler le reflow du scrollback des voisins** (complexité élevée).
   `resize_all_panes` (`src/window.rs`) est synchrone après chaque split et, sur un
   split **horizontal** (le défaut), reflow tout le `VecDeque` de scrollback des
   voisins en O(scrollback × cols), jusqu'à 10k lignes. Le nouveau pane, lui, ne
   paie jamais. Garder le resize de grille visible synchrone, différer le reflow du
   scrollback.

### Pour « le prompt utilisable instantané »

2. **Pool de shells pré-chauffés (1-2 shells pré-forkés)** (impact élevé /
   complexité élevée). Le **seul levier côté Kova**. Pré-spawner 1-2 vrais shells
   dans `$HOME` qui sourcent leur rc en tâche de fond et atteignent
   `shell_ready=true` pendant qu'on bosse ; au split, pop un shell chaud au lieu de
   forker, `cd <cwd>` si besoin. **Tension avec le principe « RAM minimale »** :
   garder N petit, jamais de capture PTY sur les shells chauds. `Pty::dummy` /
   `Pane::placeholder` ne conviennent pas (pas de vrai process).

### Quick wins indépendants

- **Capture PTY OFF par défaut** (`src/terminal/pty.rs`, l'arm `Err(_) => true`).
  Pas un gain de latence (`shell_ready` est posé avant l'écriture capture), mais
  écrit en clair sur disque **tout** ce qui transite par le PTY (y compris les
  mots de passe tapés) — c'est une expo sécurité. À passer en opt-in `=1`, ou
  ring-buffer en mémoire vidé seulement quand le bug alt-screen frappe.
- **Buffer de lecture 4096 → 16-64 KiB** (`src/terminal/pty.rs`) : coalesce les
  `read()` et les prises de lock sous gros débit. Throughput, pas ouverture.
- **Poll 100 ms → ~400 ms** (`src/terminal/pty.rs`) : −4× les réveils inactifs
  (reste sous le warn de join >500 ms au Drop).
- **Comparer cols/rows avant le `write()` lock** dans `resize_all_panes`
  (`src/window.rs`, le lock est pris inconditionnellement).

### Hors scope / honnête

- Les dotfiles (`~/.zshrc`, oh-my-zsh) dominent le time-to-prompt et **ne sont pas**
  du code Kova. Mitigation possible : pool (rank 2), ZDOTDIR warm-park opt-in, ou
  hint doc — pas d'accélération directe du sourcing.
- Le fix non-login **naïf** (retirer le `-` de `arg0`) est **UNSAFE** : les splits
  hériteraient de l'env de lancement de Kova, pas du PATH augmenté du premier shell
  (brew/node/python/rvm vivent dans `~/.zprofile`/`~/.zlogin`) → régression PATH
  quand Kova est lancé depuis le Finder. Si on poursuit le non-login, capturer
  l'env résolu du premier login shell et l'injecter dans les splits.
