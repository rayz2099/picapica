# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_picapica_global_optspecs
    string join \n c/config= h/help
end

function __fish_picapica_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_picapica_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_picapica_using_subcommand
    set -l cmd (__fish_picapica_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c picapica -n "__fish_picapica_needs_command" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_needs_command" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_needs_command" -f -a "serve" -d '常驻：数据面 + 控制 API + WebUI'
complete -c picapica -n "__fish_picapica_needs_command" -f -a "probe" -d '对所有仓库上游测速'
complete -c picapica -n "__fish_picapica_needs_command" -f -a "stats" -d '制品占用'
complete -c picapica -n "__fish_picapica_needs_command" -f -a "cache" -d '搜索或删除缓存命名空间'
complete -c picapica -n "__fish_picapica_needs_command" -f -a "completions" -d '生成 shell 补全脚本'
complete -c picapica -n "__fish_picapica_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c picapica -n "__fish_picapica_using_subcommand serve" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand serve" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand probe" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand probe" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand stats" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand stats" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and not __fish_seen_subcommand_from ls rm help" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand cache; and not __fish_seen_subcommand_from ls rm help" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and not __fish_seen_subcommand_from ls rm help" -f -a "ls" -d '列出缓存命名空间'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and not __fish_seen_subcommand_from ls rm help" -f -a "rm" -d '删除一个缓存命名空间的引用'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and not __fish_seen_subcommand_from ls rm help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from ls" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from rm" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from rm" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from help" -f -a "ls" -d '列出缓存命名空间'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from help" -f -a "rm" -d '删除一个缓存命名空间的引用'
complete -c picapica -n "__fish_picapica_using_subcommand cache; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c picapica -n "__fish_picapica_using_subcommand completions" -s c -l config -r -F
complete -c picapica -n "__fish_picapica_using_subcommand completions" -s h -l help -d 'Print help'
complete -c picapica -n "__fish_picapica_using_subcommand help; and not __fish_seen_subcommand_from serve probe stats cache completions help" -f -a "serve" -d '常驻：数据面 + 控制 API + WebUI'
complete -c picapica -n "__fish_picapica_using_subcommand help; and not __fish_seen_subcommand_from serve probe stats cache completions help" -f -a "probe" -d '对所有仓库上游测速'
complete -c picapica -n "__fish_picapica_using_subcommand help; and not __fish_seen_subcommand_from serve probe stats cache completions help" -f -a "stats" -d '制品占用'
complete -c picapica -n "__fish_picapica_using_subcommand help; and not __fish_seen_subcommand_from serve probe stats cache completions help" -f -a "cache" -d '搜索或删除缓存命名空间'
complete -c picapica -n "__fish_picapica_using_subcommand help; and not __fish_seen_subcommand_from serve probe stats cache completions help" -f -a "completions" -d '生成 shell 补全脚本'
complete -c picapica -n "__fish_picapica_using_subcommand help; and not __fish_seen_subcommand_from serve probe stats cache completions help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c picapica -n "__fish_picapica_using_subcommand help; and __fish_seen_subcommand_from cache" -f -a "ls" -d '列出缓存命名空间'
complete -c picapica -n "__fish_picapica_using_subcommand help; and __fish_seen_subcommand_from cache" -f -a "rm" -d '删除一个缓存命名空间的引用'
