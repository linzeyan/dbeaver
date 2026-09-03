import SwiftUI

/// What the server is doing, and the two ways to make it stop.
///
/// A table rather than a palette, which is where this departs from `GoToPalette`
/// and `TransferSheet`: those answer a question by name and are done, and this
/// one is read across — who, on what, for how long, running what. The columns
/// are the question.
///
/// It is opened when something is wrong, which decides two things about it.
/// Auto-refresh is off by default, because a client polling a struggling server
/// every five seconds is adding to the problem it was opened to look at. And the
/// kill buttons say which of the two they are rather than sharing one word: on a
/// server that offers both, cancelling a statement and closing a session are a
/// button apart and only one of them loses somebody's transaction.
struct ProcessesSheet: View {
    @Bindable var model: AppModel
    @FocusState private var typing: Bool

    private var rows: [ServerProcess] { model.visibleProcesses }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            table
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        .frame(width: 720, height: 420)
        .background(Theme.Surface.raised.color)
        .onExitCommand { model.closeProcesses() }
        .onAppear { typing = true }
        // One timer, restarted whenever the interval changes and cancelled with
        // the sheet. `task(id:)` rather than a stored `Timer` because SwiftUI
        // takes the cancellation with the view — a sheet dismissed while a tick
        // is in flight leaves nothing behind to fire again.
        .task(id: model.processRefresh) {
            guard let seconds = model.processRefresh else { return }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(seconds))
                guard !Task.isCancelled else { return }
                model.loadProcesses()
            }
        }
    }

    /// The filter, the refresh interval, and Refresh.
    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: "magnifyingglass")
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)
            TextField("Filter", text: $model.processFilter)
                .textFieldStyle(.plain)
                .font(Theme.Typography.body)
                .focused($typing)
                .frame(width: 200)
                .accessibilityLabel("Filter processes")

            Spacer(minLength: Theme.Space.sm)

            // "Auto-refresh" and not "Refresh": the button beside it is the
            // manual one, and two controls labelled the same word are two things
            // to read twice.
            Text("Auto-refresh")
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)
            Picker("", selection: $model.processRefresh) {
                Text("Off").tag(Optional<Int>.none)
                Text("5s").tag(Optional(5))
                Text("30s").tag(Optional(30))
            }
            .labelsHidden()
            .frame(width: 90)
            .accessibilityLabel("Refresh interval")

            Button("Refresh") { model.loadProcesses() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Accent.selection.color)
                .disabled(model.isReadingProcesses)
        }
        .padding(Theme.Space.md)
    }

    @ViewBuilder
    private var table: some View {
        if rows.isEmpty {
            // Which of the two empties this is. A server with nothing running
            // and a filter that matches nothing look identical and are not.
            Text(
                model.processFilter.isEmpty
                    ? "Nothing is running on this server."
                    : "No process matches \(model.processFilter)."
            )
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.tertiary.color)
            .padding(Theme.Space.md)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        } else {
            VStack(spacing: 0) {
                columnHeadings
                Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(rows) { process in
                            row(process)
                        }
                    }
                }
            }
        }
    }

    private var columnHeadings: some View {
        HStack(spacing: Theme.Space.sm) {
            heading("ID", width: 70)
            heading("User", width: 110)
            heading("Database", width: 110)
            heading("State", width: 130)
            heading("Time", width: 70)
            Text("Statement")
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 20)
        .accessibilityHidden(true)
    }

    private func heading(_ text: String, width: CGFloat) -> some View {
        Text(text)
            .font(Theme.Typography.micro)
            .foregroundStyle(Theme.Text.tertiary.color)
            .frame(width: width, alignment: .leading)
    }

    private func row(_ process: ServerProcess) -> some View {
        Button {
            model.selectedProcess = process.id
        } label: {
            HStack(spacing: Theme.Space.sm) {
                cell(process.id, width: 70)
                cell(process.user, width: 110)
                cell(process.database, width: 110)
                cell(process.state, width: 130)
                // The one number anybody reads down the column, so it is lined
                // up: a duration in a proportional font makes a long-running
                // statement no wider on the page than a short one.
                cell(process.duration, width: 70)
                    .monospacedDigit()
                Text(process.statement)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.xs)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                process.id == model.selectedProcess
                    ? Theme.Accent.selection.opacity(0.35).color : Color.clear
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(
            "\(process.user) on \(process.database), \(process.state), \(process.duration)")
    }

    private func cell(_ text: String, width: CGFloat) -> some View {
        Text(text)
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.primary.color)
            .lineLimit(1)
            .frame(width: width, alignment: .leading)
    }

    /// The count, whatever the last kill did, and the buttons.
    ///
    /// The buttons are drawn only where the server will do the thing they name,
    /// which is what the capability is for: a Cancel Statement on a server that
    /// only closes sessions would be a button that either does nothing or does
    /// the wrong, larger thing.
    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(AppModel.pluralized(rows.count, "process", "processes"))
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
            if !model.processReport.isEmpty {
                Text(model.processReport)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }
            Spacer(minLength: Theme.Space.sm)
            // Greyed rather than merely inert when no row is chosen. A plain
            // button given an explicit colour keeps it through `disabled`, so
            // the two most destructive controls in the window would go on
            // looking pressable while doing nothing — which reads as a bug in
            // the button rather than as an instruction to pick a row first.
            let armed = model.chosenProcess != nil
            if model.serverProcesses.cancelsStatements {
                Button("Cancel Statement") { model.endChosenProcess(.statement) }
                    .buttonStyle(.plain)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(
                        armed ? Theme.Accent.selection.color : Theme.Text.tertiary.color
                    )
                    .disabled(!armed)
            }
            if model.serverProcesses.closesSessions {
                Button("Close Session") { model.endChosenProcess(.session) }
                    .buttonStyle(.plain)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(
                        armed ? Theme.Semantic.dangerText.color : Theme.Text.tertiary.color
                    )
                    .disabled(!armed)
            }
            Button("Done") { model.closeProcesses() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 30)
    }
}
