# TODO

## Priorità alta

_(nessuna al momento)_

## Priorità media

- **Il canale live cattura tutto lo schermo invece della sola finestra/pagina Safari dedicata.** Verificato dall'utente guardando il canale da GSE: nonostante il crop sia configurato su una finestra specifica (`window_title_contains`), quello che si vede è l'intero schermo del Mac, non solo la pagina attesa. Da capire la causa (possibili piste: crop calcolato male, finestra non trovata e fallback non previsto, scala/coordinate sbagliate, o il crop giusto ma con qualcos'altro sopra che copre la finestra — vedi nota sull'occlusione in CLAUDE.md).
- **Ricaricare `channels.toml` senza riavviare il server.** Oggi ogni nuova finestra/canale live richiede di editare `channels.toml` a mano e riavviare `cargo run` perché la config si legge solo all'avvio. Idea: un endpoint o un watch sul file che ricarica i canali a caldo, senza interrompere le pipeline già attive.

## Priorità bassa

- **Distribuzione via CDN/edge relay per reggere molti spettatori remoti.** Oggi la banda di upload di casa è il vero collo di bottiglia per "tanti utenti" (HLS già disaccoppia il numero di spettatori dall'encoding — un canale si incodifica una volta sola — ma ogni spettatore in più consuma banda di upload domestica). Se un giorno il traffico cresce, la soluzione non è hardware più potente ma caricare ogni canale una sola volta su un CDN/relay (es. Cloudflare Stream, Bunny.net, o un piccolo relay self-hosted su un VPS) che poi serve tutti gli spettatori, scalando la banda lì invece che su casa.
