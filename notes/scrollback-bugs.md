# Bugs de scrollback

Suivi des problèmes liés au scroll et au scrollback buffer.

## Architecture

- `scrollback`: `VecDeque<CompactRow>` — lignes historiques (FIFO, oldest devant)
- `grid`: `Vec<Row>` — écran courant (taille fixe = `rows`)
- `scroll_offset`: `i32` — combien de lignes remonter dans le scrollback (0 = pas de scroll)
- `visible_lines()` blend scrollback + grid selon `scroll_offset`
- `push_to_scrollback()` — helper unique pour push + limit + scroll_offset adjustment

## Bugs corrigés

### Zones vides lors du scroll vers le haut

**Symptôme :** Grandes zones noires sans texte visibles en scrollant loin vers le haut.

**Cause :** `erase_in_display` mode 2/3 (`ESC[2J`) blanchissait la grille sur place sans sauver le contenu dans le scrollback. Les lignes vides étaient ensuite poussées dans le scrollback par `scroll_up` au fur et à mesure que du nouveau contenu arrivait.

**Fix :** Avant de blanquer la grille, on copie les lignes jusqu'à la dernière non-vide dans le scrollback. Les trailing blanks (bas d'écran vide) sont ignorés, mais les lignes vides intermédiaires sont préservées pour garder l'espacement visuel.

## Bugs connus / à investiguer

_(à compléter)_
