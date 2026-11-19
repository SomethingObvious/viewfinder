@{
    # Every rule at every severity, except the one below.
    IncludeDefaultRules = $true
    Severity            = @('Error', 'Warning', 'Information')

    ExcludeRules        = @(
        # install.ps1 and lint.ps1 run in a console, and their coloured lines are what
        # the person reads. Write-Output would put them in the pipeline instead.
        'PSAvoidUsingWriteHost'
    )
}
