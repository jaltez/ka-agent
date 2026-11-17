# Propuesta Nivel 2 — `CycleState`: consolidación de la frontera de ciclo

> Estado: PROPUESTA (no implementada). Revisión solicitada antes de tocar
> `events.rs`/`intake.rs`. Basada en el inventario de máquinas implícitas
> y en los tres incidentes de nov-2026.

## Problema

La frontera de ciclo (admitir prompt → trabajar → ask → abort → asentar →
drenar) tiene su estado repartido en cuatro sitios (`SessionStatus` fila+
memoria, `pending_asks`, `turn.status` en SQLite, `queue_len`) y su función
de transición escrita como predicados booleanos re-derivados a mano en 4
handlers (`rt_maybe_idle`, `handle_message_active`, drain de `Idle`,
barrido de `TurnFinished`). Cada arista nueva exige re-derivar todo el
predicado: ahí nacieron los tres incidentes.

## Diseño

Un enum dueño del ciclo, un único módulo de transiciones, `SessionStatus`
como proyección persistida (solo la máquina escribe):

```rust
enum CycleState {
    Idle,
    Handshaking { turn: TurnId },   // submit en vuelo; el turno puede existir ya
    Working    { turn: TurnId },
    AskWait    { turn: TurnId, asks: NonZeroUsize },
    Stopping   { turn: TurnId, grace_armed: bool },
    Settling   { turn: TurnId },    // entry-action: barrer huérfanos, outbox, entrega
}

impl CycleState {
    fn on(self, ev: CycleEvent) -> Result<CycleState, IllegalEdge>;
    fn accepts_prompt(&self) -> bool;   // matches!(Idle)
    fn steerable(&self) -> bool;        // matches!(Working)
    fn settled(&self) -> bool;          // sustituye el predicado de rt_maybe_idle
}
```

- `CycleEvent` = Submit / TurnStarted / Ask{..} / Answered / TurnFinished{stop}
  / Queue / Stop / EngineClosed / AskExpired.
- Las entry-actions de `Settling` absorben el barrido de huérfanos
  (v0.14.4) y el flush del outbox (v0.14.1): los parches dejan de estar
  repartidos y se vuelven la semántica del estado.
- `SessionStatus` se escribe como write-through desde la máquina; los
  lectores externos (web, /status, otras surfaces) no cambian.

## Plan de ejecución

1. `runtime/cycle.rs` con el enum + transiciones puras + matriz de aristas
   al estilo `status_machines.rs` (la compatibilidad con las 20 aristas de
   `SessionStatus` se verifica por tabla).
2. `drive()` traduce PortEvents → CycleEvents; los handlers existentes se
   reducen a efectos (persistencias, emits) ordenados por la máquina.
3. `handle_message_active`/`stop_turn`/decide/expire llaman a la máquina,
   no a predicados.
4. Migración guiada por las regresiones existentes: `orphan_ask`,
   `empty_final`, `turn_started`, `wedge_regressions`, suite de canales —
   verde antes y después de cada paso. Checker de invariantes activo todo
   el tiempo (debug).

## Fuera de alcance (explícito)

- No cambia el store, ni la API de surfaces, ni el protocolo del worker.
- La entrega (B2) y el lifecycle del worker (B3) se quedan como están;
  B2 pendiente de tu decisión pérdida-ms vs duplicado-raro.

## Coste/riesgo

2-4 días de trabajo cuidadoso en el código más caliente del repo,
protegido por: 204 tests existentes + matrices nuevas + checker de
invariantes. La alternativa (dejarlo como está) mantiene la clase de bug
que produjo los tres incidentes — ahora al menos con red de detección.
