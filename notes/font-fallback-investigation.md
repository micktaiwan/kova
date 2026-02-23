# Font Fallback — Investigation (2026-02-24)

## Ce qui a été implémenté

Fichier modifié : `src/renderer/glyph_atlas.rs`

1. **Support UTF-16 surrogate pairs** : supprimé le early return `if count != 1` qui bloquait les caractères hors BMP (emoji). On passe maintenant `count` (1 ou 2) à `glyphs_for_characters`.

2. **Font fallback via CoreText** : nouvelle méthode `resolve_glyph()` qui :
   - Essaie d'abord la police principale
   - Si elle échoue (`!ok || glyph_id == 0`), appelle `CTFont::for_string()` (= `CTFontCreateForString`) pour obtenir une police de fallback
   - Cache la fallback font dans `HashMap<char, CFRetained<CTFont>>`

3. **Imports ajoutés** : `CFRange`, `CFRetained` depuis `objc2_core_foundation`

## Ce qui fonctionne

- `printf '\u2500\u2502\u250C\u2510\n'` → box-drawing s'affichent correctement
- `printf '\e[38;5;208m╭─╮\e[0m\n'` → box-drawing en couleur OK
- Alt screen avec box-drawing (test manuel) → OK
- Emoji `echo "🎉"` → le fallback vers Apple Color Emoji fonctionne (glyph trouvé), mais le rendu est moche car on rasterise en monochrome blanc (bitmap 1-composante alpha). Les emoji couleur nécessiteraient un pipeline RGBA séparé.

## Problème restant : banner Claude Code

Le banner Claude Code (Ink/React) affiche des bordures `─` (U+2500). Investigation :

### Faits vérifiés

1. Les `─` arrivent dans `put_char` (rows 2, 6, 8) — **OK**
2. Le glyph est rasterisé (272 bytes non-zero sur bitmap 17×33) — **OK**
3. Des vertices sont générés chaque frame (3948 render logs sur ~10s) — **OK**
4. Le glyph fonctionne en mode normal (`printf '\u2500\n'`) — **OK**
5. Le glyph fonctionne en alt screen manuel (`printf '\e[?1049h...'`) — **OK**
6. Le glyph fonctionne avec couleur ANSI (`printf '\e[38;5;208m─\e[0m\n'`) — **OK**
7. Menlo a les glyphs box-drawing nativement (glyph_id=2236 pour ─) — pas besoin de fallback
8. `$TERM=xterm-256color`, `$TERM_PROGRAM=Kova` — config standard

### Observation non encore expliquée

Un log de render montre `fg=[0.1, 0.1, 0.12]` (quasi-noir) pour des `─`. **ATTENTION** : ce log a peut-être été lu depuis un fichier log stale (le filtre code était `row_idx==2 && col_idx<3` mais le log montrait `row=0, col=48`). **Il faut re-vérifier avec un fichier log propre** pour confirmer si la couleur fg est bien le problème.

### Prochaine étape pour le banner

Relancer un test propre avec Claude Code dans Kova, log vers un **nouveau fichier**, et vérifier :
- La couleur fg exacte des `─` du banner (avec filtre row/col correspondant aux lignes du banner)
- Si des cellules sont écrasées entre `put_char` et le rendu

## Problème confirmé : rendu des block elements et box-drawing

### Constat

Comparaison Kova vs autre terminal du banner Claude Code :
- **Ligne noire horizontale** qui traverse le logo (fait de block elements ▐▛█▜▌▝▘)
- **Couleurs plus sombres/désaturées** dans Kova

La ligne noire vient du fait que les glyphs de police ne remplissent pas la cellule à 100% (hinting, margins). Les block elements comme `█` sont censés couvrir toute la cellule bord à bord, mais le rendu CoreText laisse des gaps.

### Approche des terminaux modernes (vérifiée)

Les terminaux majeurs dessinent eux-mêmes les box-drawing et block elements au lieu de passer par la police :
- **Alacritty** : builtin font (commit f717710) — "font glyphs tend to overlap or not align"
- **Windows Terminal** : dessine manuellement box-drawing et powerline glyphs
- **GNOME Terminal** : bitmaps 5×5 étirés pour remplir la cellule
- **Kitty** : rendering custom pour box-drawing

### Plan d'implémentation

Dans `rasterize_char()`, pour les ranges Unicode suivants, dessiner les pixels directement dans le bitmap au lieu de passer par CoreText :

1. **Block elements** (U+2580-U+259F) — priorité haute (logo Claude)
   - `█` (U+2588) : remplir toute la cellule
   - `▌` (U+258C) : remplir la moitié gauche
   - `▐` (U+2590) : remplir la moitié droite
   - `▀` (U+2580) : remplir la moitié haute
   - `▄` (U+2584) : remplir la moitié basse
   - `▛` (U+259B), `▜` (U+259C), `▝` (U+259D), `▘` (U+2598), etc. : quadrants

2. **Box-drawing** (U+2500-U+257F) — priorité moyenne (bordures)
   - `─` (U+2500) : ligne horizontale centrée
   - `│` (U+2502) : ligne verticale centrée
   - `┌┐└┘` : coins (jonction de lignes)
   - `╭╮╰╯` : coins arrondis
   - `├┤┬┴┼` : jonctions T et croix
   - Variantes bold (━┃), double (═║╔╗╚╝), etc.

Approche : dans `rasterize_char()`, avant d'appeler `resolve_glyph()`, vérifier si le char est dans ces ranges. Si oui, remplir `bmp_buf` directement avec les pixels blancs aux bonnes positions, puis continuer le flow normal (copie atlas, upload GPU).

Ref: https://github.com/alacritty/alacritty/commit/f7177101eda589596ab08866892bd4629bd1ef44

## Logs de debug ajoutés (à retirer)

- `src/renderer/glyph_atlas.rs` : `resolve_glyph()` logge le char, utf16_len, primary/fallback results, nonzero bytes
- `src/renderer/mod.rs` : log du render des `─` avec fg/bg (filtré row_idx==2, col_idx<3)
- `src/terminal/mod.rs` : log dans `put_char` pour les box-drawing U+2500-U+257F

## État du code

Le code compile et le font fallback fonctionne. Les logs de debug sont encore présents.
