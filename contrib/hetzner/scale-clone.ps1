# SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
# SPDX-License-Identifier: MIT

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$RemoteUrl,

    [Parameter(Mandatory = $true)]
    [string]$ViewFile,

    [ValidateNotNullOrEmpty()]
    [string]$OutputRoot = (Join-Path (Get-Location) "lore-scale-results"),

    [ValidateNotNullOrEmpty()]
    [int[]]$Concurrency = @(1, 10, 40, 80),

    [ValidateRange(1, 1440)]
    [int]$StageTimeoutMinutes = 180,

    [ValidateNotNullOrEmpty()]
    [string]$LoreExecutable = "lore",

    [switch]$SkipVerify
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function ConvertTo-WindowsCommandLineArgument {
    param([AllowEmptyString()][string]$Argument)

    if ($Argument.Length -gt 0 -and $Argument -notmatch '[\s"]') {
        return $Argument
    }

    $builder = [System.Text.StringBuilder]::new()
    [void]$builder.Append('"')
    $backslashes = 0
    foreach ($character in $Argument.ToCharArray()) {
        if ($character -eq '\') {
            $backslashes++
            continue
        }
        if ($character -eq '"') {
            [void]$builder.Append(('\' * (($backslashes * 2) + 1)))
            [void]$builder.Append('"')
        } else {
            [void]$builder.Append(('\' * $backslashes))
            [void]$builder.Append($character)
        }
        $backslashes = 0
    }
    [void]$builder.Append(('\' * ($backslashes * 2)))
    [void]$builder.Append('"')
    return $builder.ToString()
}

function Start-LoreProcess {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,

        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory,

        [Parameter(Mandatory = $true)]
        [string]$LogPrefix
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $LoreExecutable
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    if ($startInfo.PSObject.Properties.Name -contains "ArgumentList") {
        foreach ($argument in $Arguments) {
            [void]$startInfo.ArgumentList.Add($argument)
        }
    } else {
        # Windows PowerShell 5.1 / .NET Framework has no ArgumentList API.
        $startInfo.Arguments = ($Arguments | ForEach-Object {
            ConvertTo-WindowsCommandLineArgument $_
        }) -join " "
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "Failed to start $LoreExecutable"
    }

    [pscustomobject]@{
        Process = $process
        StartedAt = [DateTimeOffset]::UtcNow
        StdoutTask = $process.StandardOutput.ReadToEndAsync()
        StderrTask = $process.StandardError.ReadToEndAsync()
        LogPrefix = $LogPrefix
    }
}

function Wait-LoreProcesses {
    param(
        [Parameter(Mandatory = $true)]
        [object[]]$Runs,

        [Parameter(Mandatory = $true)]
        [DateTimeOffset]$Deadline
    )

    $timedOut = $false
    while (($Runs.Process.HasExited -contains $false) -and ([DateTimeOffset]::UtcNow -lt $Deadline)) {
        Start-Sleep -Milliseconds 500
    }

    foreach ($run in $Runs) {
        if (-not $run.Process.HasExited) {
            $timedOut = $true
            $run.Process.Kill()
        }
        $run.Process.WaitForExit()
        $run.StdoutTask.GetAwaiter().GetResult() | Set-Content -Path "$($run.LogPrefix).stdout.log"
        $run.StderrTask.GetAwaiter().GetResult() | Set-Content -Path "$($run.LogPrefix).stderr.log"
    }

    return $timedOut
}

if (-not (Get-Command $LoreExecutable -ErrorAction SilentlyContinue)) {
    throw "Lore executable '$LoreExecutable' was not found on PATH"
}

$resolvedView = (Resolve-Path -LiteralPath $ViewFile).Path
$runRoot = Join-Path $OutputRoot ([DateTimeOffset]::UtcNow.ToString("yyyyMMdd-HHmmss"))
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null

$allResults = [System.Collections.Generic.List[object]]::new()
foreach ($clientCount in $Concurrency) {
    if ($clientCount -lt 1) {
        throw "Concurrency values must be positive: $clientCount"
    }

    $stageRoot = Join-Path $runRoot ("clients-{0}" -f $clientCount)
    New-Item -ItemType Directory -Path $stageRoot -Force | Out-Null
    $deadline = [DateTimeOffset]::UtcNow.AddMinutes($StageTimeoutMinutes)
    $cloneRuns = @()

    Write-Host "Starting $clientCount isolated clones into $stageRoot"
    for ($index = 1; $index -le $clientCount; $index++) {
        $clientRoot = Join-Path $stageRoot ("client-{0:D3}" -f $index)
        New-Item -ItemType Directory -Path $clientRoot -Force | Out-Null
        $clonePath = Join-Path $clientRoot "repo"
        $logPrefix = Join-Path $clientRoot "clone"
        $arguments = @("clone", "--view", $resolvedView, $RemoteUrl, $clonePath)
        $run = Start-LoreProcess -Arguments $arguments -WorkingDirectory $clientRoot -LogPrefix $logPrefix
        $run | Add-Member -NotePropertyName Client -NotePropertyValue $index
        $run | Add-Member -NotePropertyName ClonePath -NotePropertyValue $clonePath
        $cloneRuns += $run
    }

    $cloneTimedOut = Wait-LoreProcesses -Runs $cloneRuns -Deadline $deadline

    $verifyRuns = @()
    if (-not $SkipVerify -and -not $cloneTimedOut) {
        foreach ($cloneRun in $cloneRuns | Where-Object { $_.Process.ExitCode -eq 0 }) {
            $verifyRun = Start-LoreProcess `
                -Arguments @("repository", "verify", "state") `
                -WorkingDirectory $cloneRun.ClonePath `
                -LogPrefix (Join-Path (Split-Path $cloneRun.ClonePath -Parent) "verify")
            $verifyRun | Add-Member -NotePropertyName Client -NotePropertyValue $cloneRun.Client
            $verifyRuns += $verifyRun
        }
    }

    $verifyTimedOut = $false
    if ($verifyRuns.Count -gt 0) {
        $verifyTimedOut = Wait-LoreProcesses -Runs $verifyRuns -Deadline $deadline
    }

    foreach ($cloneRun in $cloneRuns) {
        $verifyRun = $verifyRuns | Where-Object { $_.Client -eq $cloneRun.Client } | Select-Object -First 1
        $allResults.Add([pscustomobject]@{
            concurrency = $clientCount
            client = $cloneRun.Client
            clone_exit_code = $cloneRun.Process.ExitCode
            clone_seconds = [Math]::Round(($cloneRun.Process.ExitTime.ToUniversalTime() - $cloneRun.StartedAt.UtcDateTime).TotalSeconds, 3)
            verify_exit_code = if ($null -eq $verifyRun) { $null } else { $verifyRun.Process.ExitCode }
            verify_seconds = if ($null -eq $verifyRun) { $null } else { [Math]::Round(($verifyRun.Process.ExitTime.ToUniversalTime() - $verifyRun.StartedAt.UtcDateTime).TotalSeconds, 3) }
            stage_timed_out = $cloneTimedOut -or $verifyTimedOut
            clone_path = $cloneRun.ClonePath
        })
    }

    $stageResults = $allResults | Where-Object { $_.concurrency -eq $clientCount }
    $failed = @($stageResults | Where-Object { $_.clone_exit_code -ne 0 -or ($null -ne $_.verify_exit_code -and $_.verify_exit_code -ne 0) })
    Write-Host ("Completed {0} clients: {1} failures" -f $clientCount, $failed.Count)
    if ($failed.Count -gt 0 -or $cloneTimedOut -or $verifyTimedOut) {
        Write-Warning "Stopping the ladder after correctness or timeout failure at concurrency $clientCount"
        break
    }
}

$resultsPath = Join-Path $runRoot "results.csv"
$allResults | Export-Csv -Path $resultsPath -NoTypeInformation
Write-Host "Results: $resultsPath"
