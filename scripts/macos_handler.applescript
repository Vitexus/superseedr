-- SPDX-FileCopyrightText: 2025 The superseedr Contributors
-- SPDX-License-Identifier: GPL-3.0-or-later

on run
    return
end run

-- Handle the kInternetEventClass/kAEGetURL Apple event used for URL schemes.
on «event GURLGURL» this_URL
    process_link(this_URL)
end «event GURLGURL»

on open these_files
    repeat with this_file in these_files
        process_link(POSIX path of this_file)
    end repeat
end open

on process_link(the_link)
    set link_to_process to the_link as text
    if link_to_process is not "" then
        try
            set binary_path_posix to "/usr/local/bin/superseedr"
            set full_command to (quoted form of binary_path_posix) & " " & (quoted form of link_to_process)
            «event sysoexec» (full_command & " > /dev/null 2>&1 &")
        on error
            return
        end try
    end if
end process_link
