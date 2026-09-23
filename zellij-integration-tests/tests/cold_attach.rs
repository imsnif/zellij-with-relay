#![cfg(unix)]

use zellij_integration_tests::{
    claim_first_terminal_and_wait_for_prompt, start_zellij, FakePtyHandle, Size, TERMINAL_SIZE,
};

const ATTACHING_CLIENT_SIZE: Size = Size { cols: 80, rows: 30 };
const LARGER_ATTACHING_CLIENT_SIZE: Size = Size {
    cols: 160,
    rows: 40,
};

fn pane_size(terminal: &FakePtyHandle, what: &str) -> (u16, u16) {
    terminal.wait_for_size(what, |_, _| true)
}

#[test]
fn cold_attach_fits_both_clients_without_querying_their_size() {
    let mut zellij = start_zellij();
    let terminal = claim_first_terminal_and_wait_for_prompt(&zellij);
    let (initial_cols, initial_rows) = pane_size(&terminal, "initial pane size");

    let attaching_client = zellij.attach_client(ATTACHING_CLIENT_SIZE);
    attaching_client.wait_until("attaching client loaded", |grid_snapshot| {
        grid_snapshot.tab_bar_appears()
            && grid_snapshot.contains("Ctrl +")
            && grid_snapshot.cursor.is_some()
    });

    let (attached_cols, attached_rows) = terminal
        .wait_for_size("pane resized to fit both clients", |cols, rows| {
            (cols, rows) != (initial_cols, initial_rows)
        });

    let chrome_rows = TERMINAL_SIZE.rows as u16 - initial_rows;
    let expected_rows =
        std::cmp::min(TERMINAL_SIZE.rows, ATTACHING_CLIENT_SIZE.rows) as u16 - chrome_rows;
    let expected_cols = std::cmp::min(TERMINAL_SIZE.cols, ATTACHING_CLIENT_SIZE.cols) as u16;

    assert_eq!(
        attached_cols, expected_cols,
        "layout must be applied at the narrowest client's width"
    );
    assert_eq!(
        attached_rows, expected_rows,
        "layout must be applied at the shortest client's height"
    );

    let attaching_client_messages = attaching_client.received_server_messages();
    assert!(
        !attaching_client_messages
            .iter()
            .any(|name| name == "QueryTerminalSize"),
        "a cold attach must not be asked for its terminal size, got: {:?}",
        attaching_client_messages
    );

    let main_client_messages = zellij.received_server_messages();
    assert!(
        !main_client_messages
            .iter()
            .any(|name| name == "QueryTerminalSize"),
        "an existing client must not be asked for its terminal size when a peer attaches, got: {:?}",
        main_client_messages
    );

    attaching_client.quit();
    zellij.quit();
}

#[test]
fn attaching_a_larger_client_does_not_overflow_the_existing_terminal() {
    let mut zellij = start_zellij();
    let terminal = claim_first_terminal_and_wait_for_prompt(&zellij);
    let (initial_cols, initial_rows) = pane_size(&terminal, "initial pane size");

    let attaching_client = zellij.attach_client(LARGER_ATTACHING_CLIENT_SIZE);
    attaching_client.wait_until("larger attaching client loaded", |grid_snapshot| {
        grid_snapshot.tab_bar_appears()
            && grid_snapshot.status_bar_appears()
            && grid_snapshot.cursor.is_some()
    });

    terminal.output(b"after-attach");
    let main_grid = zellij.wait_until(
        "main client rendered after the larger client attached",
        |grid_snapshot| grid_snapshot.contains("after-attach"),
    );

    assert_eq!(
        main_grid.row_of_line("Tab #1"),
        Some(0),
        "the tab bar must stay on the first row of the existing client, got:\n{}",
        main_grid
    );
    assert!(
        main_grid.status_bar_appears(),
        "the status bar must stay visible on the existing client, got:\n{}",
        main_grid
    );

    assert_eq!(
        pane_size(&terminal, "pane size after the larger client attached"),
        (initial_cols, initial_rows),
        "a larger attaching client must not grow the shared pane"
    );
    let oversized_sizes: Vec<(u16, u16)> = terminal
        .size_history()
        .into_iter()
        .filter(|(cols, rows)| *cols > initial_cols || *rows > initial_rows)
        .collect();
    assert!(
        oversized_sizes.is_empty(),
        "the shared pane must never be sized beyond the smallest client, got: {:?}",
        oversized_sizes
    );

    attaching_client.wait_until(
        "attaching client still renders its own chrome",
        |grid_snapshot| {
            grid_snapshot.tab_bar_appears()
                && grid_snapshot.status_bar_appears()
                && grid_snapshot.contains("after-attach")
        },
    );

    attaching_client.quit();
    zellij.quit();
}
