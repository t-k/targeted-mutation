# Evaluation on past fixes

A mutation score is not comparable between tools whose mutants differ in difficulty. The question asked here is a different one: **would the tool have pointed at the code a bug fix later had to change, given only the code and the tests as they were before the fix?**

`past-fixes.sh` answers it for a repository's history:

1. Take a commit `C` that fixes a bug in a package.
2. Check out `C^`: the code and the tests of the time. Nothing the fix adds is visible.
3. Run `tmt --since C`. On the pre-fix checkout, "changed since `C`" is exactly the set of lines the fix is going to touch.
4. A finding or unreached code on those lines means the tests left room for the bug there.

```text
eval/past-fixes.sh <clone of the repository> <output directory> <tmt binary> <list>
```

`<list>` has one `commit package` pair per line. Use a clone, not your working tree: the script checks out old commits. The output directory gets one JSON report per commit and `summary.tsv`.

Reading the summary:

- `sites = 0`: the lines the fix changed hold nothing the tool mutates. Typical cases are fixes that only add new code, or that change a constant table, a message or a type. These commits say nothing about the tool either way and should be reported separately.
- `sites > 0` and no finding: a miss. The tool looked at the code and the tests of the time killed every mutant it tried.
- `sites > 0` and a finding: the tests of the time let a mutant of the later-fixed code through.

A hit is not proof that the reported mutant *is* the bug, only that the place was under-tested in a way the tool can see. Read a few before quoting a rate.
