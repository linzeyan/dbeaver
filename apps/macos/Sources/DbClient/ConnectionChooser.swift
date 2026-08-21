import SwiftUI

/// What the window shows: the connection chooser until a database is open, the
/// session after that.
///
/// One window and one hosting view rather than a sheet over the shell. A sheet
/// is a separate `NSWindow`, which puts it outside every capture of the main
/// one — and screenshots are how layout defects get caught in this project. It
/// also matches what switching connections means here: the panes behind the
/// chooser describe a database that is about to stop being the one on screen.
struct RootView: View {
    @Bindable var model: AppModel

    var body: some View {
        Group {
            if model.isPresentingConnection {
                ConnectionChooser(model: model)
            } else {
                MainView(model: model)
            }
        }
        // The swap happens with no animation, which is not a style choice.
        // SwiftUI cross-fades an `if`/`else` between two view trees by keeping
        // both alive for the duration, and `MainView` contains an
        // `NSViewRepresentable` over a Metal layer. The card's layer was
        // surviving that fade stranded behind the grid, visible as a dark
        // rectangle wherever the grid had no rows to draw over it — and
        // invisible on a full table, which is why it took a four-row view to
        // find. Nothing here is worth animating anyway: the chooser and the
        // session share no element for a transition to carry between them.
        .transaction { $0.animation = nil }
    }
}

/// Where a database is chosen.
///
/// Two panes in the geometry the session already uses, so that connecting does not
/// relayout the window: the connections somebody keeps down the left, the one they
/// are looking at on the right. The alternative — the form alone, which is what this
/// was — asks a person who works with four databases to retype one of them every
/// time, and gives them nowhere to record which of the four is the dangerous one.
///
/// The list is never empty of rows. Quick connect is always its first, which keeps
/// "a connection I am not keeping" from being a mode: it is a row, it is selected
/// like a row, and it holds its own draft while somebody looks at another.
struct ConnectionChooser: View {
    @Bindable var model: AppModel
    @FocusState private var focus: FocusArea?

    /// Which folders are shut.
    ///
    /// `@State` and nothing else, which is the whole of "local, not synced": it
    /// is not in `connections.json`, so a file carried to another machine does
    /// not carry one person's idea of which folders are interesting. The cost is
    /// that it resets when the chooser is put away — opening it is a deliberate
    /// act and every folder open is the right thing to see on the way in.
    @State private var shutFolders: Set<String> = []

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 200, ideal: 240, max: 320)
        } detail: {
            form
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.background.color)
        }
        .task {
            // Where reading starts, and never the password field — which is
            // what the form used to do when the other fields were already
            // filled in.
            //
            // That one line was what put AppKit's AutoFill panel on screen. It
            // is not summoned by having a secure field, it is summoned by a
            // secure field holding focus: measured four ways against this
            // window — the panel follows the focus and nothing else, the same
            // build showing it and not showing it with only this line changed.
            // So a form that focused the password field was asking for the
            // panel before the user had asked for anything, which is the one
            // thing about it nobody chose.
            //
            // It costs a remembered connection one Tab. What it buys is a form
            // that opens as itself, and an AutoFill panel that appears when
            // somebody puts the caret in the password field — which is the
            // moment it is worth having. `buttons` keeps the band it lands in
            // for that moment.
            focus = .connectHost
        }
    }

    // MARK: - The list

    private var sidebar: some View {
        VStack(spacing: 0) {
            ConnectionFilterField(text: $model.connectionFilter, focus: $focus)
                .padding(.horizontal, Theme.Space.sm)
                .padding(.vertical, Theme.Space.sm)

            QuickConnectRow(
                subtitle: quickConnectSubtitle,
                isSelected: model.selectedConnectionID == nil,
                select: { model.selectConnection(nil) },
                connect: model.connectFromForm
            )
            .padding(.horizontal, Theme.Space.xs)

            Rectangle()
                .fill(Theme.separator.color)
                .frame(height: 1)
                .padding(.horizontal, Theme.Space.sm)
                .padding(.vertical, Theme.Space.xs)

            list
        }
        // Opaque by default for the reason `NavigatorView` gives at length: the
        // detail column's backgrounds run under the sidebar and its vibrancy
        // samples them. The same setting decides it here, because a translucency
        // that applied to one of this application's two sidebars and not the other
        // would read as a defect in whichever one was noticed second.
        .background(
            model.preferences.usesTranslucentSidebar ? Color.clear : Theme.background.color
        )
        .safeAreaInset(edge: .bottom) { footer }
    }

    @ViewBuilder
    private var list: some View {
        if model.connections.connections.isEmpty {
            // Not "no connections": there is one directly above this, and it works.
            // What is missing is the file, and the sentence says how a row gets
            // into it.
            EmptyState(
                symbol: "square.stack.3d.up.slash",
                title: "No saved connections",
                hint: "Fill in the form and press Save to keep one.")
        } else if model.visibleConnections.isEmpty {
            EmptyState(
                symbol: "magnifyingglass",
                title: "No matches",
                hint: "No connection is named like “\(model.connectionFilter)”.")
        } else {
            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(model.visibleConnectionGroups) { group in
                        // The top level has no header. A heading over the
                        // connections nobody filed would be naming a folder that
                        // does not exist, and it would be the first thing in a
                        // sidebar where most people never make a folder at all.
                        if !group.path.isEmpty {
                            FolderHeader(
                                name: group.name,
                                count: group.connections.count,
                                isShut: shutFolders.contains(group.path),
                                toggle: {
                                    if shutFolders.contains(group.path) {
                                        shutFolders.remove(group.path)
                                    } else {
                                        shutFolders.insert(group.path)
                                    }
                                }
                            )
                        }
                        if group.path.isEmpty || !shutFolders.contains(group.path) {
                            ForEach(group.connections) { connection in
                                ConnectionRow(
                                    connection: connection,
                                    isSelected: model.selectedConnectionID == connection.id,
                                    isOpen: isOpen(connection),
                                    hasUnsavedEdits: model.selectedConnectionID == connection.id
                                        && model.unsavedConnectionEdits != nil,
                                    select: { model.selectConnection(connection.id) },
                                    connect: model.connectFromForm
                                )
                            }
                        }
                    }
                }
                .padding(.horizontal, Theme.Space.xs)
                .padding(.bottom, Theme.Space.xs)
            }
        }
    }

    /// Whether the session waiting behind this window is on that connection.
    ///
    /// Compared without the password, which is the only part of the string that is
    /// not in the file — a comparison including it would call a connection closed
    /// the moment somebody retyped its password, which is the moment it is most
    /// obviously open.
    private func isOpen(_ connection: SavedConnection) -> Bool {
        model.openConnectionSettings == connection.settings
    }

    /// What Quick connect's row says under its name: what is in the draft, or where
    /// the draft is not kept. Never a blank line, which reads as a row still loading.
    private var quickConnectSubtitle: String {
        guard model.selectedConnectionID == nil else { return "Not kept in the file" }
        let subtitle = model.connectionDraft.subtitle
        return subtitle.isEmpty ? "Not kept in the file" : subtitle
    }

    /// New, delete, and how many there are — the band, tone and density the
    /// navigator's footer uses, because it answers the same two questions: what
    /// this list holds, and what can be done to it.
    private var footer: some View {
        HStack(spacing: Theme.Space.xs) {
            Button(action: model.newConnection) {
                Image(systemName: "plus")
                    .font(.system(size: 10, weight: .medium))
                    .frame(width: 18, height: 16)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.textSecondary.color)
            .help("Empty the Quick connect form")
            .accessibilityLabel("New connection")

            Button(action: model.deleteConnection) {
                Image(systemName: "minus")
                    .font(.system(size: 10, weight: .medium))
                    .frame(width: 18, height: 16)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            // Coloured rather than left to the button style's dimming, which does
            // not read at 10pt on a dark background.
            .foregroundStyle(
                model.canDeleteConnection ? Theme.textSecondary.color : Theme.textTertiary.color
            )
            .disabled(!model.canDeleteConnection)
            .help("Delete the selected connection")
            .accessibilityLabel("Delete connection")

            Spacer()

            Text(countLabel)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.textTertiary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.xs + 2)
        .background(Theme.surface.color)
        .overlay(alignment: .top) {
            Rectangle().fill(Theme.separator.color).frame(height: 1)
        }
    }

    private var countLabel: String {
        let total = model.connections.connections.count
        let shown = model.visibleConnections.count
        return shown == total
            ? AppModel.pluralized(total, "connection")
            : "\(shown) of \(AppModel.pluralized(total, "connection"))"
    }

    // MARK: - The form

    private var form: some View {
        VStack(alignment: .leading, spacing: Theme.Space.lg) {
            header

            // Above the fields, which is where the detail pane keeps its banner
            // too — and it has to be above the password field in particular,
            // because AppKit's AutoFill button (see `buttons`) is drawn directly
            // under that one and would cover the sentence explaining why the
            // chooser is still on screen.
            if let message = model.connectionError {
                InlineBanner(message: message) { model.connectionError = nil }
            }

            VStack(spacing: Theme.Space.sm) {
                // Above the fields that say where the database is, because it is
                // the answer to a different question: not which database this is,
                // but which one it is to the person opening it.
                row("Name", $model.connectionDraft.name, .connectName, "optional")
                // Beside the name rather than down with the address, because it
                // answers the same kind of question: not which database this is,
                // but where the person opening it keeps it. Typed as a path —
                // there is no folder to pick from until somebody has made one, and
                // a picker that is empty on a fresh install is a control that
                // teaches nothing.
                row("Folder", $model.connectionDraft.folder, .connectFolder, "top level")
                colourRow
                driverRow
                // The fields a database actually needs. A file has no host, no
                // port and nobody to authenticate as, and showing four disabled
                // boxes beside a path would be describing the form rather than
                // the database.
                if model.connectionDraft.settings.driver?.shape == .file {
                    row(
                        "File", $model.connectionDraft.settings.path, .connectDatabase,
                        "/path/to/file.db")
                } else {
                    HStack(spacing: Theme.Space.sm) {
                        label("Host")
                        field(
                            $model.connectionDraft.settings.host, .connectHost, "127.0.0.1",
                            named: "Host")
                        label("Port", width: 32)
                        field(
                            $model.connectionDraft.settings.port, .connectPort, portPlaceholder,
                            named: "Port"
                        )
                        .frame(width: 56)
                    }
                    row("Database", $model.connectionDraft.settings.database, .connectDatabase, "")
                    row("User", $model.connectionDraft.settings.user, .connectUser, "")
                    // An empty secure field says "no password saved". Where one
                    // is saved and simply has not been read, the placeholder is
                    // what stops the form from saying something untrue.
                    row(
                        "Password", $model.connectionPassword, .connectPassword,
                        model.hasUnreadPassword ? "Saved" : "", isSecure: true)
                }
                // Only for a driver that reads them, which is what keeps this
                // from being a control with no effect — and the effect it would
                // appear to be claiming is whether anybody on the network can
                // read the wire.
                //
                // Above Safety and below the address, because it belongs with
                // them: those fields say which database this is and this says
                // how to reach it, where Safety says what may be done to it.
                if model.connectionDraft.settings.driver?.honoursSslMode == true {
                    sslRow
                    // Only where naming one would change something. Under
                    // Require the field would sit there taking a path that is
                    // never read, which reads as a certificate being checked.
                    if model.connectionDraft.settings.sslMode.verifiesCertificate {
                        row(
                            "CA", $model.connectionDraft.settings.sslRootCert, .connectRootCert,
                            "public roots only")
                    }
                }
                // Outside the branch above, because both answers to it can be
                // marked: a SQLite file somebody is not to write to is as real a
                // thing as a production server.
                safetyRow
                if let test = model.connectionTest {
                    testRow(test)
                }
            }

            buttons
        }
        .padding(Theme.Space.lg)
        .frame(width: 420)
        .background(
            RoundedRectangle(cornerRadius: Theme.Radius.card, style: .continuous)
                .fill(Theme.surface.color)
        )
        .overlay(
            RoundedRectangle(cornerRadius: Theme.Radius.card, style: .continuous)
                .strokeBorder(Theme.separator.color, lineWidth: 1)
        )
    }

    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: "cylinder.split.1x2")
                .font(.system(size: 18, weight: .light))
                .foregroundStyle(Theme.accent.color)
            // No subtitle. It used to name the one dialect the core spoke, then
            // counted them — and "15 databases", sitting directly above a form
            // whose next row asks which database to open, reads as fifteen
            // databases available to connect to. How many kinds this build
            // supports is answered by the picker below, where it is a choice
            // rather than a boast.
            Text("Connect to a database")
                .font(Theme.Typography.title)
                .foregroundStyle(Theme.text.color)
        }
        .accessibilityElement(children: .combine)
    }

    /// A band, a hairline, then the buttons.
    ///
    /// The band is not generous padding. A focused secure field is given an
    /// AutoFill affordance by macOS: an `SPRoundedWindow` out of
    /// SafariPlatformSupport, hosting a remote view, 104×37pt, laid flush under
    /// the field and aligned to its leading edge. It belongs to another process
    /// — its size, its tone and its corner radius are not this card's to set —
    /// and nothing on the field refuses it: a hand-built `NSSecureTextField`
    /// with a nil `contentType` and `isAutomaticTextCompletionEnabled` off gets
    /// one too, as does a bare `SecureField` in an otherwise empty window of
    /// this application. Dropping `SecureField` would refuse it and would also
    /// give up secure text entry, which is not a trade a password field makes.
    ///
    /// So the one thing left to decide is where it lands, and this decides it.
    /// The 16pt the card puts between its sections plus this 24 is 40 — the 37
    /// the affordance occupies, plus a hair — which is why the hairline is far
    /// enough down that it cannot be reached, and why the buttons below it are
    /// right-aligned for no reason other than that being where a Mac dialog
    /// keeps them.
    ///
    /// The band is empty most of the time, and that is the point rather than a
    /// waste: the form does not open with the password field focused (see
    /// `body`), so the affordance appears only once somebody puts the caret
    /// there. Reserving the space is what keeps that moment from shoving a
    /// system control across the button row.
    private var buttons: some View {
        VStack(spacing: 0) {
            Rectangle()
                .fill(Theme.separator.color)
                .frame(height: 1)
                .padding(.top, Theme.Space.xl)
                .padding(.bottom, Theme.Space.md)

            HStack(spacing: Theme.Space.sm) {
                Spacer(minLength: 0)

                // No word beside it: the spinner sits next to a Connect button
                // that is disabled for exactly as long, and the two together
                // already say what is happening.
                if model.isConnecting { ProgressView().controlSize(.small) }

                // Only while there is something to go back to. A Revert on a form
                // that already matches what was saved is a button that does
                // nothing, and a row of those teaches people not to read the row.
                if model.unsavedConnectionEdits != nil {
                    Button("Revert", action: model.revertConnection)
                        .help("Put back the values this connection was saved with")
                }

                // Before Save, because it is the question somebody asks *of* a
                // form they are not sure about — and left of the two buttons that
                // change something, since it changes nothing.
                Button("Test", action: model.testConnection)
                    .disabled(!model.canTestConnection)
                    .help("Open this connection, ask what answered, and close it again")

                Button("Save", action: model.saveConnection)
                    .disabled(!model.canSaveConnection)
                    .help("Keep this connection in the file")

                // Cancel is offered only once there is a session to go back to.
                // At launch there is nothing behind this window, and a Cancel that
                // leads to an empty window is a button that breaks the
                // application.
                if model.canCancelConnection {
                    Button("Cancel") { model.cancelConnection() }
                        .keyboardShortcut(.cancelAction)
                }

                Button("Connect") { model.connectFromForm() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    // The window's own accent rather than the system's, which is
                    // whatever the user picked in Settings and need not belong
                    // beside this palette.
                    .tint(Theme.accent.color)
                    .disabled(!model.canConnect)
            }
        }
    }

    /// The database being connected to.
    ///
    /// A menu rather than a segmented control: the list is fifteen entries, and
    /// a segmented control that long is unreadable at exactly the point it
    /// matters.
    private var driverRow: some View {
        HStack(spacing: Theme.Space.sm) {
            // "Kind", not "Database". This row and the one three below it were
            // both labelled DATABASE, meaning two different things — which kind
            // of server to speak to, and which database on it to open — so the
            // form read top to bottom as DATABASE, HOST, PORT, DATABASE. The
            // accessibility label already said "Database kind" and was the only
            // place the distinction was drawn; "Kind" is what fits the 62pt
            // label column, and it is the word this window already uses for
            // what an object is (`RelationKind`).
            label("Kind")
            Picker("", selection: driverBinding) {
                ForEach(DriverCatalog.all) { driver in
                    Text(driver.label).tag(driver.scheme)
                }
                // A connection string naming a driver this build does not have
                // keeps its scheme rather than being quietly reassigned, so the
                // picker has to be able to show it. Listed only when it happens.
                if model.connectionDraft.settings.driver == nil {
                    Text("\(model.connectionDraft.settings.scheme) (not in this build)")
                        .tag(model.connectionDraft.settings.scheme)
                }
            }
            .labelsHidden()
            .accessibilityLabel("Database kind")
            Spacer(minLength: 0)
        }
    }

    /// What the last Test found, in the fields' own alignment so that it reads as
    /// another line of the form rather than as something laid over it.
    ///
    /// Its own row rather than the error banner at the top of the card. That
    /// banner is where a failed *connect* goes and it can be dismissed, because
    /// it describes something that stopped happening; a test result describes
    /// something that was asked for and answered, and it stays until the next
    /// question.
    ///
    /// The three states borrow the dot the window already uses for a connection,
    /// rather than three new symbols meaning the same three things.
    @ViewBuilder
    private func testRow(_ test: AppModel.ConnectionTest) -> some View {
        HStack(spacing: Theme.Space.sm) {
            label("")
            switch test {
            case .running:
                StatusDot(state: .connecting)
                Text("Testing…").foregroundStyle(Theme.textSecondary.color)
            case .reached(let info):
                StatusDot(state: .connected)
                Text(info.label).foregroundStyle(Theme.text.color)
            case .failed(let message):
                StatusDot(state: .failed)
                Text(message)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(2)
            }
            Spacer(minLength: 0)
        }
        .font(Theme.Typography.caption)
        .accessibilityElement(children: .combine)
    }

    /// How much of the server's identity to insist on.
    ///
    /// A picker rather than a checkbox, because the answer is not on or off. The
    /// two useful middles — encrypt without proving anything, and prove the
    /// chain without the name — are exactly the ones a checkbox cannot say, and
    /// the second is the only way to reach a server by address or through a
    /// tunnel without turning verification off altogether.
    ///
    /// The sentence beside it is not decoration. "Require" sounds like the strict
    /// setting and is the one that accepts any certificate at all; a form that
    /// showed libpq's word without saying what it does would be handing on the
    /// most misread option in every PostgreSQL client there is.
    private var sslRow: some View {
        HStack(spacing: Theme.Space.sm) {
            label("SSL")
            Picker("", selection: $model.connectionDraft.settings.sslMode) {
                ForEach(SslMode.allCases) { mode in
                    Text(mode.title).tag(mode)
                }
            }
            .labelsHidden()
            .frame(width: 120)
            Text(model.connectionDraft.settings.sslMode.summary)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.textSecondary.color)
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .font(Theme.Typography.body)
        .foregroundStyle(Theme.text.color)
    }

    /// What this connection is allowed to be.
    ///
    /// Two checkboxes rather than one control with three settings, because they
    /// are not exclusive and the pair that proves it is the ordinary case: a
    /// production database somebody is browsing with edits switched off. A
    /// control that made them exclusive would be inventing a rule the flags do
    /// not have.
    ///
    /// Below the fields that say where the database is, since it is a different
    /// kind of answer — those describe the database, and these describe what this
    /// application may do to it.
    private var safetyRow: some View {
        HStack(spacing: Theme.Space.sm) {
            label("Safety")
            Toggle("Read-only", isOn: $model.connectionDraft.isReadOnly)
                .help("Refuse grid edits, generated DDL and imports on this connection")
            Toggle("Production", isOn: $model.connectionDraft.isProduction)
                .help("Ask before writing to this connection")
            Spacer(minLength: 0)
        }
        .font(Theme.Typography.body)
        .foregroundStyle(Theme.text.color)
    }

    /// The mark that tells one server from another at a glance.
    ///
    /// Eight swatches rather than a colour well: the point is that two people
    /// looking at the same connection see the same red, and a well offers millions
    /// of tones, most of which are invisible on this background. The one that means
    /// "no colour" sits with the rest because it is how somebody undoes a colour
    /// they did not mean to pick.
    private var colourRow: some View {
        HStack(spacing: Theme.Space.sm) {
            label("Colour")
            HStack(spacing: Theme.Space.xs + 1) {
                ForEach(ConnectionColor.allCases) { colour in
                    ColourSwatch(
                        colour: colour,
                        isSelected: model.connectionDraft.color == colour
                    ) {
                        model.connectionDraft.color = colour
                    }
                }
            }
            Spacer(minLength: 0)
        }
    }

    /// Changing the picker moves the draft rather than replacing it, so that
    /// switching database does not empty a form somebody has been typing into.
    private var driverBinding: Binding<String> {
        Binding(
            get: { model.connectionDraft.settings.scheme },
            set: { scheme in
                guard let driver = DriverCatalog.named(scheme) else { return }
                model.connectionDraft.settings = model.connectionDraft.settings.moved(to: driver)
            })
    }

    /// The port the chosen database listens on by default, shown greyed until
    /// the field is filled. Empty for one with no default.
    private var portPlaceholder: String {
        model.connectionDraft.settings.driver?.defaultPort.map(String.init) ?? ""
    }

    private func row(
        _ name: String, _ text: Binding<String>, _ area: FocusArea, _ placeholder: String,
        isSecure: Bool = false
    ) -> some View {
        HStack(spacing: Theme.Space.sm) {
            label(name)
            field(text, area, placeholder, named: name, isSecure: isSecure)
        }
    }

    /// Fixed width and trailing-aligned, so the fields line up into a column
    /// rather than stepping in and out with the length of each word.
    private func label(_ name: String, width: CGFloat = 62) -> some View {
        FieldLabel(text: name)
            .frame(width: width, alignment: .trailing)
    }

    private func field(
        _ text: Binding<String>, _ area: FocusArea, _ placeholder: String, named name: String,
        isSecure: Bool = false
    ) -> some View {
        CompactField(
            placeholder: placeholder, text: text, area: area, focus: $focus,
            // ↩ in any field is Connect. Reaching for the button after typing a
            // password is a step nobody wants in the one window that stands
            // between them and their data.
            onSubmit: model.connectFromForm, isSecure: isSecure
        )
        // `FieldLabel` is hidden from accessibility — it is decoration beside
        // the control — so the name has to be said here or the field is
        // announced by its placeholder, which for Host is a bare IP address.
        .accessibilityLabel(name)
    }
}

// MARK: - Rows

/// A folder's name, and the disclosure that shuts it.
///
/// A row rather than SwiftUI's `DisclosureGroup`, which draws its own indentation
/// and its own chevron at its own size — and this sidebar's rows are already a
/// fixed height with a colour stripe down the left. Two conventions in one column
/// is the thing a reader has to work out instead of reading.
///
/// The count is on the header because a shut folder is otherwise a line that says
/// nothing about what shutting it hid.
private struct FolderHeader: View {
    let name: String
    let count: Int
    let isShut: Bool
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: Theme.Space.xs) {
                Image(systemName: isShut ? "chevron.right" : "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textTertiary.color)
                    .frame(width: 10)
                Text(name)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
                Spacer(minLength: Theme.Space.xs)
                Text("\(count)")
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                    .monospacedDigit()
            }
            .padding(.horizontal, Theme.Space.sm)
            .frame(height: 24)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(name), \(count) connections")
        .accessibilityAddTraits(isShut ? [] : [.isSelected])
    }
}

/// One saved connection.
///
/// Two lines, because one is not enough to tell two databases on the same server
/// apart and three is a row nobody scans. The title truncates at its end and the
/// subtitle in its middle: the end of `user@host:port/database` is the part that
/// distinguishes two rows, so it is the part that has to survive a narrow sidebar.
private struct ConnectionRow: View {
    let connection: SavedConnection
    let isSelected: Bool
    let isOpen: Bool
    let hasUnsavedEdits: Bool
    let select: () -> Void
    let connect: () -> Void

    @State private var isHovering = false

    var body: some View {
        Button(action: select) {
            HStack(spacing: Theme.Space.sm) {
                stripe
                VStack(alignment: .leading, spacing: 1) {
                    Text(connection.title)
                        .font(Theme.Typography.bodyEmphasis)
                        .foregroundStyle(Theme.text.color)
                        .lineLimit(1)
                    Text(connection.subtitle)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.textSecondary.color)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: Theme.Space.xs)
                marker
            }
            .padding(.horizontal, Theme.Space.sm)
            .frame(height: 40)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.control).fill(fill)
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.control))
        }
        .buttonStyle(.plain)
        .onHover { isHovering = $0 }
        // Double-click opens it, which is what a list of things to open is for.
        // The click that selects is not wasted work: the form has to be showing the
        // connection about to be opened, or a failure would land on the wrong row.
        .simultaneousGesture(TapGesture(count: 2).onEnded { connect() })
        .help("\(connection.title) — \(connection.subtitle)")
        .accessibilityLabel(accessibilityLabel)
        .accessibilityAddTraits(isSelected ? [.isSelected, .isButton] : .isButton)
    }

    /// Reserved whether or not there is a colour, so that the names in a list of
    /// mixed rows still line up.
    private var stripe: some View {
        RoundedRectangle(cornerRadius: 1.5, style: .continuous)
            .fill(connection.color.tone?.color ?? .clear)
            .frame(width: 3, height: 24)
    }

    /// One mark asking the reader to do something, or else everything that is
    /// true of the connection.
    ///
    /// The unsaved pencil still wins alone: of everything shown here it is the
    /// only thing asking for a decision, and a row cannot ask two. The rest are
    /// facts and are shown together — a deliberate change to the one-mark rule
    /// this row used to keep, because a production connection that stopped being
    /// marked as one the moment it was opened would be hiding the mark exactly
    /// when it matters.
    @ViewBuilder
    private var marker: some View {
        if hasUnsavedEdits {
            Image(systemName: "pencil")
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(Theme.warning.color)
                .accessibilityHidden(true)
        } else {
            HStack(spacing: Theme.Space.xs) {
                if connection.isReadOnly {
                    Image(systemName: "lock")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Theme.textTertiary.color)
                        .accessibilityHidden(true)
                }
                if connection.isProduction {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Theme.danger.color)
                        .accessibilityHidden(true)
                }
                if isOpen {
                    StatusDot(state: .connected)
                }
            }
        }
    }

    /// The name, then what it opens, then whatever else is true of it — in the
    /// order somebody reading the row would meet them.
    private var accessibilityLabel: String {
        var parts = [connection.title, connection.subtitle]
        if connection.color != .none { parts.append(connection.color.label) }
        // Before the open dot and the pencil, because these two are the reason
        // somebody would stop at this row rather than a detail about its state.
        if connection.isReadOnly { parts.append("Read-only") }
        if connection.isProduction { parts.append("Production") }
        if hasUnsavedEdits { parts.append("Unsaved changes") }
        if isOpen { parts.append("Open") }
        return parts.joined(separator: ", ")
    }

    /// The hover fill is deliberately weaker than the selected fill: hovering must
    /// never be mistakable for "this is the connection I am on". The same rule
    /// `TabButton` states, in the same two tones.
    private var fill: Color {
        if isSelected { return Theme.accent.opacity(0.30).color }
        return isHovering ? Theme.surfaceRaised.color : .clear
    }
}

/// The row for a connection nobody is keeping.
///
/// First, and always there. Somebody who wants to reach a database once should not
/// have to decide whether to add it to a list they will later have to tidy — and
/// without this row the only way to type into the form would be to create an entry
/// or to edit one that already means something.
private struct QuickConnectRow: View {
    let subtitle: String
    let isSelected: Bool
    let select: () -> Void
    let connect: () -> Void

    @State private var isHovering = false

    var body: some View {
        Button(action: select) {
            HStack(spacing: Theme.Space.sm) {
                // The width a colour stripe occupies, empty. Quick connect cannot
                // carry a colour, but starting its title 6pt left of every other
                // title made the list read as two lists — and the eye finds that
                // long before it finds the reason.
                Color.clear.frame(width: 3, height: 24)
                VStack(alignment: .leading, spacing: 1) {
                    Text("Quick connect")
                        .font(Theme.Typography.bodyEmphasis)
                        .foregroundStyle(Theme.text.color)
                    Text(subtitle)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.textSecondary.color)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: Theme.Space.xs)
                // In the slot the saved rows keep for their pencil and their open
                // dot, because that is the only place left where it costs no
                // alignment. It says the same thing there: what is different about
                // this row.
                Image(systemName: "bolt.horizontal.circle")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textTertiary.color)
                    .accessibilityHidden(true)
            }
            .padding(.horizontal, Theme.Space.sm)
            .frame(height: 40)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.control).fill(fill)
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.control))
        }
        .buttonStyle(.plain)
        .onHover { isHovering = $0 }
        .simultaneousGesture(TapGesture(count: 2).onEnded { connect() })
        .help("A connection this Mac does not keep")
        .accessibilityLabel("Quick connect, \(subtitle)")
        .accessibilityAddTraits(isSelected ? [.isSelected, .isButton] : .isButton)
    }

    private var fill: Color {
        if isSelected { return Theme.accent.opacity(0.30).color }
        return isHovering ? Theme.surfaceRaised.color : .clear
    }
}

// MARK: - Controls used only here

/// One of the eight colours a connection can carry.
///
/// A button rather than a tappable circle, so that it has the role and can say its
/// own name: the colour is the one thing about a connection that a screen reader
/// cannot work out from anything else on screen.
private struct ColourSwatch: View {
    let colour: ConnectionColor
    let isSelected: Bool
    let choose: () -> Void

    var body: some View {
        Button(action: choose) {
            swatch
                .frame(width: 14, height: 14)
                .overlay(
                    Circle()
                        .strokeBorder(isSelected ? Theme.accent.color : .clear, lineWidth: 2)
                        .padding(-3)
                )
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .help(colour.label)
        .accessibilityLabel(colour.label)
        .accessibilityAddTraits(isSelected ? [.isSelected, .isButton] : .isButton)
    }

    @ViewBuilder
    private var swatch: some View {
        if let tone = colour.tone {
            Circle().fill(tone.color)
        } else {
            // Drawn rather than left blank: a gap in a row of swatches reads as one
            // that failed to load, and this is the one somebody reaches for to take
            // a colour back off.
            Circle()
                .strokeBorder(Theme.border.color, lineWidth: 1)
                .overlay(
                    Image(systemName: "xmark")
                        .font(.system(size: 7, weight: .bold))
                        .foregroundStyle(Theme.textTertiary.color)
                )
        }
    }
}

/// Name filter for the connection list.
///
/// Its own view rather than `SidebarFilterField`, which belongs to the session: that
/// one names ⌥⌘F in its tooltip and searches schemas and tables. A control that told
/// somebody about a shortcut belonging to a window they are not in is worse than one
/// with no tooltip at all.
private struct ConnectionFilterField: View {
    @Binding var text: String
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        HStack(spacing: Theme.Space.xs + 2) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(Theme.textTertiary.color)

            TextField("Filter connections", text: $text)
                .textFieldStyle(.plain)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.text.color)
                .focused($focus, equals: .connectionFilter)

            if !text.isEmpty {
                Button {
                    text = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.textTertiary.color)
                }
                .buttonStyle(.plain)
                .help("Clear filter (⎋)")
                .accessibilityLabel("Clear filter")
            }
        }
        .padding(.horizontal, Theme.Space.sm)
        .frame(height: 22)
        .background(
            RoundedRectangle(cornerRadius: Theme.Radius.control)
                .fill(Theme.background.opacity(0.6).color)
        )
        .overlay(
            RoundedRectangle(cornerRadius: Theme.Radius.control)
                .strokeBorder(
                    focus == .connectionFilter ? Theme.accent.color : Theme.separator.color,
                    lineWidth: 1)
        )
        // Escape empties the field, which is the reflex every macOS search field
        // trains — and a filter left switched on by accident is a list that looks
        // as though connections have gone missing.
        .onExitCommand { text = "" }
        .help("Show only connections whose name or address contains this")
    }
}
