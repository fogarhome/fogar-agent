import AppKit
import SwiftUI

/// Fogar Desktop: the main window (sidebar + sections), Settings (⌘,) and the menu bar quick view.
/// One `AgentLink` for all of them: one subscription to the agent, no polling.
@main
struct FogarDesktopApp: App {
    @State private var link: AgentLink
    @State private var nav = Navegacion()
    @AppStorage("apariencia") private var apariencia = Apariencia.sistema

    init() {
        Tipo.registrar()
        let l = AgentLink()
        l.start()
        _link = State(initialValue: l)
    }

    var body: some Scene {
        Window("Fogar", id: "main") {
            RootView()
                .environment(link)
                .environment(nav)
        }
        .defaultSize(width: 1100, height: 740)
        .windowStyle(.hiddenTitleBar)
        .commands { FogarCommands(link: link, nav: nav) }

        Settings {
            AjustesView(enVentana: false)
                .environment(link)
                .environment(nav)
        }
        .windowResizability(.contentSize)

        MenuBarExtra {
            MenuBarView()
                .environment(link)
                .environment(nav)
        } label: {
            // The label lives as long as the app: it applies the appearance to every window,
            // the menu bar panel included (preferredColorScheme does not reach that panel).
            MenuBarIcono(state: link.state, retirado: link.accesoRetirado)
                .onChange(of: apariencia, initial: true) { _, a in NSApp.appearance = a.nsAppearance }
        }
        .menuBarExtraStyle(.window)
    }
}

enum Seccion: String, CaseIterable, Identifiable {
    case estado, emparejar, ajustes
    var id: Self { self }

    var titulo: String {
        switch self {
        case .estado: "Estado"
        case .emparejar: "Emparejar"
        case .ajustes: "Ajustes"
        }
    }

    var icono: String {
        switch self {
        case .estado: "point.3.connected.trianglepath.dotted"
        case .emparejar: "link"
        case .ajustes: "gearshape"
        }
    }

    var atajo: KeyEquivalent {
        switch self {
        case .estado: "1"
        case .emparejar: "2"
        case .ajustes: "3"
        }
    }
}

/// The selected section, shared by the window, the menus and the menu bar view. Restored at launch.
@Observable
final class Navegacion {
    var seccion: Seccion {
        didSet { UserDefaults.standard.set(seccion.rawValue, forKey: "seccion") }
    }

    init() {
        seccion = UserDefaults.standard.string(forKey: "seccion").flatMap(Seccion.init(rawValue:)) ?? .estado
    }
}

enum Apariencia: String, CaseIterable, Identifiable {
    case sistema, oscura, clara
    var id: Self { self }
    var nsAppearance: NSAppearance? {
        switch self {
        case .sistema: nil
        case .oscura: NSAppearance(named: .darkAqua)
        case .clara: NSAppearance(named: .aqua)
        }
    }

    var titulo: String {
        switch self {
        case .sistema: "Automática"
        case .oscura: "Oscura"
        case .clara: "Clara"
        }
    }
}

struct FogarCommands: Commands {
    let link: AgentLink
    let nav: Navegacion
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .newItem) {}
        CommandGroup(before: .sidebar) {
            ForEach(Seccion.allCases) { s in
                Button(s.titulo) {
                    nav.seccion = s
                    openWindow(id: "main")
                }
                .keyboardShortcut(s.atajo, modifiers: .command)
            }
            Divider()
            Button("Volver a llamar al agente") { link.retryNow() }
                .keyboardShortcut("r", modifiers: .command)
            Divider()
        }
    }
}
