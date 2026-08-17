import SwiftUI

/// What the window shows: the connection form until a database is open, the
/// session after that.
///
/// One window and one hosting view rather than a sheet over the shell. A sheet
/// is a separate `NSWindow`, which puts it outside every capture of the main
/// one — and screenshots are how layout defects get caught in this project. It
/// also matches what switching connections means here: the panes behind the
/// form describe a database that is about to stop being the one on screen.
struct RootView: View {
    @Bindable var model: AppModel

    var body: some View {
        Group {
            if model.isPresentingConnection {
                ConnectView(model: model)
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
        // find. Nothing here is worth animating anyway: the form and the
        // session share no element for a transition to carry between them.
        .transaction { $0.animation = nil }
    }
}

/// Where a database is chosen.
///
/// Five fields rather than a libpq string box: `--conn` is the string-shaped
/// entry point and it belongs to scripts. Someone opening a client wants to
/// change the database name, not to edit a keyword list — and the fields are
/// what let the password be handled separately from everything else, which is
/// the whole reason it can stay out of UserDefaults.
struct ConnectView: View {
    @Bindable var model: AppModel
    @FocusState private var focus: FocusArea?

    var body: some View {
        card
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Theme.background.color)
            .task {
                // The field most likely to be the one that is wrong. A
                // remembered connection needs only its password; an empty form
                // starts where reading starts.
                focus = model.connectionDraft.isComplete ? .connectPassword : .connectHost
            }
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: Theme.Space.lg) {
            header

            // Above the fields, which is where the detail pane keeps its banner
            // too — and it has to be above the password field in particular,
            // because AppKit's AutoFill button (see `footer`) is drawn directly
            // under that one and would cover the sentence explaining why the
            // form is still on screen.
            if let message = model.connectionError {
                InlineBanner(message: message) { model.connectionError = nil }
            }

            VStack(spacing: Theme.Space.sm) {
                driverRow
                // The fields a database actually needs. A file has no host, no
                // port and nobody to authenticate as, and showing four disabled
                // boxes beside a path would be describing the form rather than
                // the database.
                if model.connectionDraft.driver?.shape == .file {
                    row("File", $model.connectionDraft.path, .connectDatabase, "/path/to/file.db")
                } else {
                    HStack(spacing: Theme.Space.sm) {
                        label("Host")
                        field(
                            $model.connectionDraft.host, .connectHost, "127.0.0.1", named: "Host")
                        label("Port", width: 32)
                        field(
                            $model.connectionDraft.port, .connectPort, portPlaceholder,
                            named: "Port"
                        )
                        .frame(width: 56)
                    }
                    row("Database", $model.connectionDraft.database, .connectDatabase, "")
                    row("User", $model.connectionDraft.user, .connectUser, "")
                    row("Password", $model.connectionPassword, .connectPassword, "", isSecure: true)
                }
            }

            footer
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

    /// Buttons right-aligned, which is where a Mac dialog keeps them — and it
    /// has to be here for a second reason. AppKit reads a secure field as a
    /// website login and hangs an AutoFill "Passwords…" button underneath it,
    /// aligned to the field's leading edge; there is no supported way to refuse
    /// one. Leaving that half of the row empty is what keeps it from drawing
    /// over Cancel.
    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Spacer(minLength: 0)

            // No word beside it: the spinner sits next to a Connect button that
            // is disabled for exactly as long, and the two together already say
            // what is happening.
            if model.isConnecting { ProgressView().controlSize(.small) }

            // Cancel is offered only once there is a session to go back to. At
            // launch there is nothing behind this form, and a Cancel that leads
            // to an empty window is a button that breaks the application.
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
                if model.connectionDraft.driver == nil {
                    Text("\(model.connectionDraft.scheme) (not in this build)")
                        .tag(model.connectionDraft.scheme)
                }
            }
            .labelsHidden()
            .accessibilityLabel("Database kind")
            Spacer(minLength: 0)
        }
    }

    /// Changing the picker moves the draft rather than replacing it, so that
    /// switching database does not empty a form somebody has been typing into.
    private var driverBinding: Binding<String> {
        Binding(
            get: { model.connectionDraft.scheme },
            set: { scheme in
                guard let driver = DriverCatalog.named(scheme) else { return }
                model.connectionDraft = model.connectionDraft.moved(to: driver)
            })
    }

    /// The port the chosen database listens on by default, shown greyed until
    /// the field is filled. Empty for one with no default.
    private var portPlaceholder: String {
        model.connectionDraft.driver?.defaultPort.map(String.init) ?? ""
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
            // password is a step nobody wants in the one dialog that stands
            // between them and their data.
            onSubmit: model.connectFromForm, isSecure: isSecure
        )
        // `FieldLabel` is hidden from accessibility — it is decoration beside
        // the control — so the name has to be said here or the field is
        // announced by its placeholder, which for Host is a bare IP address.
        .accessibilityLabel(name)
    }
}
