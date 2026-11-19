---------------------------- MODULE WorkCopy ----------------------------
(* The working copy across runs, for one file (files do not interact).

   A run brings the copy up to date, writes the instrumented text of the files it
   instruments, and builds. To let cargo skip the build of what did not change, a file
   that an earlier run instrumented is left alone by the sync: the run renders it again
   from the source and writes only when the text differs. Which files those are is kept
   in a log beside the copy. A run can be interrupted anywhere, and between runs the
   user edits or deletes the source and asks for other things to be instrumented.

   Checked: when a run reaches the build, the copy holds exactly what the run wants,
   whatever earlier runs left behind. *)
EXTENDS Naturals

CONSTANT MaxVersion

VARIABLES
  src,       \* version of the source file; 0: it does not exist
  dst,       \* <<kind, version>> in the copy, kind in {"none", "plain", "instr"}
  logged,    \* the log says an earlier or this run wrote the file
  pc,        \* "idle", "synced", "logging", "written", "restored", "build"
  selected,  \* this run instruments the file
  wrote      \* this run wrote the file

vars == <<src, dst, logged, pc, selected, wrote>>

None == <<"none", 0>>

Init ==
  /\ src = 1
  /\ dst = None
  /\ logged = FALSE
  /\ pc = "idle"
  /\ selected = FALSE
  /\ wrote = FALSE

(* Between runs. *)
Edit ==
  /\ pc = "idle"
  /\ src' \in 0..MaxVersion
  /\ UNCHANGED <<dst, logged, pc, selected, wrote>>

(* Sync and prune. A logged file that is there is kept as it is, even when the source
   is gone: only this run knows whether it writes it again. *)
Sync ==
  /\ pc = "idle"
  /\ selected' \in IF src = 0 THEN {FALSE} ELSE BOOLEAN
  /\ wrote' = FALSE
  /\ dst' = IF logged /\ dst # None THEN dst
            ELSE IF src = 0 THEN None
            ELSE <<"plain", src>>
  /\ pc' = "synced"
  /\ UNCHANGED <<src, logged>>

(* The log entry is made before the write. *)
Log ==
  /\ pc = "synced"
  /\ IF selected
       THEN logged' = TRUE /\ pc' = "logging"
       ELSE logged' = logged /\ pc' = "written"
  /\ UNCHANGED <<src, dst, selected, wrote>>

Write ==
  /\ pc = "logging"
  /\ dst' = <<"instr", src>>   \* written only if the text differs: same result
  /\ wrote' = TRUE
  /\ pc' = "written"
  /\ UNCHANGED <<src, logged, selected>>

(* What the log names and this run did not write goes back to the source. *)
Restore ==
  /\ pc = "written"
  /\ dst' = IF logged /\ ~wrote
              THEN IF src = 0 THEN None ELSE <<"plain", src>>
              ELSE dst
  /\ pc' = "restored"
  /\ UNCHANGED <<src, logged, selected, wrote>>

(* Only then does the log shrink to what this run wrote. *)
Trim ==
  /\ pc = "restored"
  /\ logged' = wrote
  /\ pc' = "build"
  /\ UNCHANGED <<src, dst, selected, wrote>>

Finish ==
  /\ pc = "build"
  /\ pc' = "idle"
  /\ UNCHANGED <<src, dst, logged, selected, wrote>>

Interrupt ==
  /\ pc \notin {"idle", "build"}
  /\ pc' = "idle"
  /\ UNCHANGED <<src, dst, logged, selected, wrote>>

Next == Edit \/ Sync \/ Log \/ Write \/ Restore \/ Trim \/ Finish \/ Interrupt

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ src \in 0..MaxVersion
  /\ dst[1] \in {"none", "plain", "instr"}
  /\ pc \in {"idle", "synced", "logging", "written", "restored", "build"}

Wanted == IF src = 0 THEN None
          ELSE IF selected THEN <<"instr", src>> ELSE <<"plain", src>>

BuildSeesWhatTheRunWants == pc = "build" => dst = Wanted

(* Instrumented text never stays in the copy without the log knowing. *)
InstrumentedIsLogged == dst[1] = "instr" => logged
=============================================================================
