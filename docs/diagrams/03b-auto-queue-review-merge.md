# Auto-Queue: 리뷰 사이클 + 머지

```plantuml
@startuml auto-queue-review-merge
!theme plain
skinparam ActivityFontSize 11
skinparam NoteFontSize 10

title Auto-Queue — 리뷰 & 머지

|#LightGreen|JS (Policy Engine)|

|JS (Policy Engine)|
start
:onReviewEnter fires;

if (Review enabled?) then (no)
  :Card -> done;
  stop
else (yes)
  if (Counter-model?) then (no)
    :Auto-approve -> done;
    stop
  else (yes)
    :Create review dispatch;
    :Agent reviews code;

    if (Verdict?) then (pass)
      if (Next pipeline stage?) then (yes)
        :Dispatch next stage;
        detach
      else (no)
        :Card -> done;
      endif
    elseif (rework) then
      if (Repeated findings?) then (yes)
        if (Session reset tried?) then (yes)
          :Escalate -> PMD;
          detach
        else (no)
          :Rework with approach change;
        endif
      else (no)
        :Create rework dispatch;
      endif
      :Card -> in_progress
      Re-enters review;
      detach
    else (no verdict)
      :Create review-decision dispatch;
      detach
    endif
  endif
endif

|JS (Policy Engine)|
:No policy merge. Review pass dispatches create-pr
(review-automation.js), then an agent babysits the PR,
merging after CI and review pass and repairing on failure;

stop

@enduml
```
