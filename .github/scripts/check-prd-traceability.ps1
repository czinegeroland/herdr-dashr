param(
    [Parameter(Mandatory = $true)]
    [string]$BaseSha,

    [Parameter(Mandatory = $true)]
    [string]$HeadSha,

    [Parameter(Mandatory = $true)]
    [string]$EventPath
)

$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    Write-Error $Message
    exit 1
}

function Normalize-Text([string]$Text) {
    return $Text.Replace("`r`n", "`n").Replace("`r", "`n")
}

function Get-Section([string]$Text, [string]$Heading) {
    $start = $Text.IndexOf($Heading, [StringComparison]::Ordinal)
    if ($start -lt 0) {
        return $null
    }

    $next = $Text.IndexOf("`n## ", $start + $Heading.Length, [StringComparison]::Ordinal)
    if ($next -lt 0) {
        return $Text.Substring($start)
    }

    return $Text.Substring($start, $next - $start)
}

function Get-PrimaryRequirements([string]$Text) {
    $functional = Get-Section $Text '## 8. Functional requirements'
    $security = Get-Section $Text '## 9. Security requirements'
    if ($null -eq $functional -or $null -eq $security) {
        Fail 'Primary requirement sections are missing.'
    }

    return "$functional`n$security"
}

$mergeBase = ((& git merge-base $BaseSha $HeadSha) -join '').Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($mergeBase)) {
    Fail 'Unable to determine the pull request merge base.'
}

$changedFiles = @(& git diff --name-only $mergeBase $HeadSha)
if ($LASTEXITCODE -ne 0) {
    Fail 'Unable to determine changed files.'
}

# A dependency update is exempt, and narrowly so.
#
# The living-PRD rule exists to keep a behaviour change traceable to the
# requirement it serves. A version bump serves no requirement: there is no
# row for it to move and no evidence for it to record, so a bot that opens
# one cannot satisfy the rule and would be blocked forever. Enabling
# Dependabot without this made every dependency pull request permanently
# unmergeable.
#
# Two conditions, both required, because an exemption is a hole:
#
#   * The author is Dependabot itself. This comes from the event payload
#     that GitHub populates, not from anything inside the pull request, so a
#     contributor cannot claim it.
#   * Every changed path is a dependency manifest. A bump that reached into
#     `crates/` or `docs/` is not a bump, whoever opened it, and goes
#     through the ordinary rule.
#
# Review is not waived by any of this: CODEOWNERS still covers every path,
# and a bump of a crate carrying a cryptographic or canonicalization
# guarantee is exactly the kind a human should read.
$dependencyManifests = @(
    'Cargo.toml',
    'Cargo.lock',
    '.github/dependabot.yml'
)

$eventForAuthor = Get-Content $EventPath -Raw | ConvertFrom-Json
$author = [string]$eventForAuthor.pull_request.user.login
$authorType = [string]$eventForAuthor.pull_request.user.type

$isDependabot = ($author -eq 'dependabot[bot]') -and ($authorType -eq 'Bot')
$onlyManifests = $changedFiles.Count -gt 0 -and -not (
    $changedFiles | Where-Object {
        ($dependencyManifests -notcontains $_) -and
        ($_ -notlike '.github/workflows/*')
    }
)

if ($isDependabot -and $onlyManifests) {
    Write-Host "PRD traceability: dependency update by $author touching only dependency manifests; the living-PRD rule does not apply."
    exit 0
}

if ($changedFiles -notcontains 'docs/PRD.md') {
    Fail 'Every pull request must update docs/PRD.md.'
}

$prd = Normalize-Text ((& git show "${HeadSha}:docs/PRD.md") -join "`n")
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($prd)) {
    Fail 'Unable to read docs/PRD.md from the pull request head.'
}

$updatedMatch = [regex]::Match(
    $prd,
    '\| Last updated \| (\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:Z|[+-]\d{2}:\d{2})) \|'
)
if (-not $updatedMatch.Success) {
    Fail 'docs/PRD.md must contain an ISO 8601 Last updated timestamp.'
}

$basePrd = $null
& git cat-file -e "${mergeBase}:docs/PRD.md" 2>$null
if ($LASTEXITCODE -eq 0) {
    $basePrd = Normalize-Text ((& git show "${mergeBase}:docs/PRD.md") -join "`n")
    $baseUpdatedMatch = [regex]::Match(
        $basePrd,
        '\| Last updated \| (\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:Z|[+-]\d{2}:\d{2})) \|'
    )

    if ($baseUpdatedMatch.Success -and
        $baseUpdatedMatch.Groups[1].Value -ceq $updatedMatch.Groups[1].Value) {
        Fail 'The PRD Last updated timestamp must change in every pull request.'
    }

    $baseLedger = Get-Section $basePrd '## 14. Delivery ledger'
    $headLedger = Get-Section $prd '## 14. Delivery ledger'
    if ($null -eq $headLedger) {
        Fail 'The PRD delivery ledger section is missing.'
    }

    if ($baseLedger -ceq $headLedger) {
        Fail 'Every pull request must update the PRD delivery ledger.'
    }
}

$event = Get-Content $EventPath -Raw | ConvertFrom-Json
$body = Normalize-Text ([string]$event.pull_request.body)
if ([string]::IsNullOrWhiteSpace($body)) {
    Fail 'The pull request body must contain PRD traceability information.'
}

$bodyWithoutComments = [regex]::Replace($body, '(?s)<!--.*?-->', '')
$requirementMatches = [regex]::Matches(
    $bodyWithoutComments,
    '\bDASHR-[A-Z]+-\d{3}\b'
)
$requirementIds = @(
    $requirementMatches |
        ForEach-Object { $_.Value } |
        Sort-Object -Unique
)

$noProgressMatch = [regex]::Match(
    $bodyWithoutComments,
    '(?im)^No requirement progress:[ \t]*(?<reason>[^\r\n]+)$'
)
$noProgressReason = if ($noProgressMatch.Success) {
    $noProgressMatch.Groups['reason'].Value.Trim()
} else {
    ''
}
$validNoProgress = (
    $noProgressReason.Length -ge 12 -and
    $noProgressReason -notmatch '(?i)^(none|n/a|not applicable)[.!]?$'
)

if ($requirementIds.Count -eq 0 -and -not $validNoProgress) {
    Fail 'List at least one real DASHR requirement ID or provide a concrete No requirement progress rationale.'
}

foreach ($requirementId in $requirementIds) {
    $headRequirements = Get-PrimaryRequirements $prd
    $baseRequirements = if ($null -ne $basePrd) {
        Get-PrimaryRequirements $basePrd
    } else {
        $null
    }

    $rowPattern = '(?m)^\|\s*' + [regex]::Escape($requirementId) + '\s*\|[^\r\n]*$'
    $headRow = [regex]::Match($headRequirements, $rowPattern)
    if (-not $headRow.Success) {
        Fail "Pull request references unknown requirement $requirementId."
    }

    $baseRow = if ($null -ne $baseRequirements) {
        [regex]::Match($baseRequirements, $rowPattern)
    } else {
        $null
    }

    if ($null -ne $baseRow -and
        $baseRow.Success -and
        $baseRow.Value -ceq $headRow.Value) {
        Fail "Requirement $requirementId is referenced, but its PRD row was not updated."
    }
}

$primaryRows = [regex]::Matches(
    (Get-PrimaryRequirements $prd),
    '(?m)^\|\s*(DASHR-[A-Z]+-\d{3})\s*\|[^\r\n]*$'
)
$duplicateIds = @(
    $primaryRows |
        ForEach-Object { $_.Groups[1].Value } |
        Group-Object |
        Where-Object { $_.Count -gt 1 } |
        ForEach-Object { $_.Name }
)
if ($duplicateIds.Count -gt 0) {
    Fail "Duplicate primary requirement IDs: $($duplicateIds -join ', ')."
}

# Requirement DASHR-GOV-003: a row
# may not claim `Verified` without citing something. The rule was written
# down and nothing enforced it, which is the state in which a ledger starts
# drifting from the thing it describes.
#
# `Implemented` is deliberately not checked here. It means the code exists;
# `Verified` means someone can point at the evidence, and that is the
# transition worth guarding.
$verifiedWithoutEvidence = @()
foreach ($row in $primaryRows) {
    $cells = @($row.Value.Trim('|').Split('|') | ForEach-Object { $_.Trim() })

    # The two primary sections do not share a layout. Section 12 rows carry a
    # priority column and section 25 rows do not, so the status is the
    # second-to-last cell and the evidence the last one in both. Indexing
    # from the front would silently read the wrong column for every security
    # requirement, which is the kind of bug a checker quietly passes with.
    if ($cells.Count -lt 4) { continue }

    $requirementId = $cells[0]
    $status = $cells[$cells.Count - 2]
    $evidence = $cells[$cells.Count - 1]

    if ($status -ne 'Verified') { continue }

    # Evidence has to name something a reader can open: a path, a test, or a
    # decision. Prose alone is how 'Verified' becomes a word rather than a
    # claim.
    $citesSomething = ($evidence -match '`[^`]+`') -or
        ($evidence -match 'DEC-\d{3}') -or
        ($evidence -match 'AC-[A-Z-]+')

    if ([string]::IsNullOrWhiteSpace($evidence) -or
        $evidence -eq 'Pending' -or
        -not $citesSomething) {
        $verifiedWithoutEvidence += $requirementId
    }
}

if ($verifiedWithoutEvidence.Count -gt 0) {
    Fail ("These requirements claim Verified without citing stable evidence: " +
        ($verifiedWithoutEvidence -join ', ') +
        ". A Verified row must reference a file, a test, a decision, or an acceptance criterion.")
}

Write-Output 'PRD traceability checks passed.'
