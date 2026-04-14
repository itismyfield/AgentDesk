# Auto-Queue: Phase Gate + 완료 + 에러 복구

```plantuml
@startuml auto-queue-continuation
!theme plain
skinparam ActivityFontSize 11
skinparam NoteFontSize 10

title Auto-Queue — Phase Gate & 복구

|#LightGreen|JS (Policy Engine)|
|#LightBlue|Rust (API / DB)|

|JS (Policy Engine)|
start
:onCardTerminal fires
(auto-queue.js);

:Find done entry for card;

if (Done entry in active run?) then (no)
  detach
else (yes)
endif

:continueRunAfterEntry();

if (Phase done?) then (yes)
  if (Phase gate required?) then (yes)
    :Create phase-gate dispatches
    Pause run
    Save gate state to kv_meta;

    :Agent evaluates gate;

    if (All gates pass?) then (yes)
      if (Final phase?) then (yes)
        :completeRunAndNotify();
        stop
      else (no)
        :Clear gate state
        Resume -> activateRun();
        detach
      endif
    else (fail)
      :Pause run
      Notify PMD;
      detach
    endif
  else (no gate)
    :activateRun() next phase;
    detach
  endif
else (no)
endif

if (Remaining == 0?) then (yes)
  |Rust (API / DB)|
  :Run -> completed
  Release all slots
  Notify channels;
  stop
else (no)
endif

|JS (Policy Engine)|
if (Group has entries?) then (yes)
  :activateRun(doneGroup);
else (group done)
  :activateRun(null)
  Start next group;
endif

detach

== onTick1min — Error Recovery ==

|JS (Policy Engine)|
partition "Recovery" {
  :Terminal pending -> skip;
  :Stale runs -> finalize;
  :Idle agents -> activateRun();
  :Stuck dispatched (>2min)
  -> reset to pending;
}

:onCardTransition
External progress -> skip entry;

stop

@enduml
```
