import FogarWire
import SwiftUI

/// Iniciar sesión: the dashboard's Conectar screen for this Mac. On one side the brand (living
/// background, living line, one line of purpose); on the other, the only things the person types:
/// where the gateway is (found or by hand), their user and their password. Everything else the
/// agent sets. The password lives only in this view's state and is cleared after each attempt.
struct IniciarSesionView: View {
    @Environment(AgentLink.self) private var link

    var body: some View {
        HStack(spacing: 0) {
            Marca()
                .zIndex(-1)
                .frame(minWidth: 300, maxWidth: .infinity, maxHeight: .infinity)
                .ignoresSafeArea(edges: .top)
            ScrollView {
                Group {
                    switch link.state {
                    case .live(let s) where s.phase == .notPaired:
                        Formulario()
                    case .live(let s):
                        YaConSesion(status: s)
                    default:
                        SinAgente(state: link.state)
                    }
                }
                .frame(maxWidth: 420, alignment: .leading)
                .padding(.horizontal, 48)
                .padding(.top, 108) // clears the title bar, then the dashboard's 56
                .padding(.bottom, 56)
                .frame(maxWidth: .infinity, minHeight: 560, alignment: .leading)
            }
            .frame(minWidth: 420, maxWidth: .infinity)
            .modifier(SinBordeDeScroll())
        }
        .ignoresSafeArea(edges: .top)
        .navigationTitle("Iniciar sesión")
    }
}

private struct Marca: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 48) {
            LineaViva(pose: .rest, etiqueta: "Fogar")
                .frame(maxWidth: 440)
                .padding(.leading, -24)
            Spacer(minLength: 0)
            Text("Fogar Desktop une este Mac a tu casa: el agente habla con tu gateway y esta app te cuenta cómo va.")
                .font(Tipo.texto(14)).suave()
                .frame(maxWidth: 360, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 56)
        .padding(.top, 108)
        .padding(.bottom, 56)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background {
            // Bleeds past the panel like the dashboard's (inset -10% -20% -30% -30%); the mask fades it.
            FondoVivo(calor: 0.45, humo: 0.9)
                .padding(.top, -80).padding(.bottom, -200).padding(.leading, -200).padding(.trailing, -160)
                .ignoresSafeArea()
        }
    }
}

private struct Formulario: View {
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav

    @State private var candidatos: [WireCandidate] = []
    @State private var buscando = false
    @State private var direccion = ""
    @State private var usuario = ""
    /// Only here, never logged or kept: emptied after every attempt and when the view goes away.
    @State private var contrasena = ""
    @State private var errorDireccion: String?
    @State private var errorCredenciales: String?
    @State private var errorGeneral: String?
    @State private var entrando = false
    @FocusState private var focoDireccion: Bool
    @FocusState private var focoUsuario: Bool
    @FocusState private var focoContrasena: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            VStack(alignment: .leading, spacing: 12) {
                if link.accesoRetirado {
                    let t = Textos.titular(link.state, retirado: true)
                    Estado(tono: t.tono, texto: t.titulo)
                        .font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
                    Text(t.sub).font(Tipo.cuerpo).foregroundStyle(Color.hueso)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.bottom, 12)
                }
                Text("Hola. Inicia sesión en este Mac.").titularFogar()
                    .fixedSize(horizontal: false, vertical: true)
                Text("Con tu usuario y tu contraseña de Fogar, los mismos de la web. La contraseña solo viaja cifrada: por https o por Tailscale.")
                    .font(Tipo.cuerpo).suave()
                    .fixedSize(horizontal: false, vertical: true)
            }

            encontrados

            Campo(etiqueta: "Dirección del gateway", pista: "Su dirección https:// o la de Tailscale (termina en .ts.net).", error: errorDireccion, campo: {
                TextField("", text: $direccion, prompt: Text("https://casa.ejemplo.com o casa.tu-red.ts.net").foregroundStyle(Color.huesoTenue))
                    .autocorrectionDisabled()
                    .onChange(of: direccion) { errorDireccion = nil }
                    .onSubmit { focoUsuario = true }
            }, enfocado: $focoDireccion)

            Campo(etiqueta: "Usuario", error: nil, campo: {
                TextField("", text: $usuario, prompt: Text("tu usuario").foregroundStyle(Color.huesoTenue))
                    .textContentType(.username)
                    .autocorrectionDisabled()
                    .onChange(of: usuario) { errorCredenciales = nil; errorGeneral = nil }
                    .onSubmit { focoContrasena = true }
            }, enfocado: $focoUsuario)

            Campo(etiqueta: "Contraseña", error: errorCredenciales, campo: {
                SecureField("", text: $contrasena, prompt: Text("tu contraseña").foregroundStyle(Color.huesoTenue))
                    .textContentType(.password)
                    // Typing clears the error; the app emptying the field after an attempt must not.
                    .onChange(of: contrasena) { _, nueva in if !nueva.isEmpty { errorCredenciales = nil; errorGeneral = nil } }
                    .onSubmit(entrar)
            }, enfocado: $focoContrasena)

            if let errorGeneral {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    EstadoIcono(tono: .bad, tam: 12)
                    Text(errorGeneral).font(Tipo.cuerpo).foregroundStyle(Color.hueso)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .accessibilityElement(children: .combine)
            }

            // Busy, not disabled: the label keeps full contrast; `entrar()` ignores repeats.
            Button(action: entrar) {
                Text(entrando ? "Iniciando sesión…" : "Iniciar sesión")
            }
            .buttonStyle(Pildora(variante: .principal, ocupado: entrando, ancho: true))
            .keyboardShortcut(.defaultAction)
        }
        .task { await buscar() }
        .onDisappear { contrasena = "" }
    }

    @ViewBuilder private var encontrados: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Gateways encontrados").font(Tipo.etiqueta).foregroundStyle(Color.hueso)
                Spacer()
                Button(buscando ? "Buscando…" : "Buscar otra vez") { Task { await buscar() } }
                    .buttonStyle(Pildora(pequena: true, ocupado: buscando))
            }
            if candidatos.isEmpty {
                Text(buscando ? "Mirando en la dirección guardada, en este Mac y en tu tailnet…"
                              : "No he encontrado ninguno. Escribe su dirección aquí abajo.")
                    .meta()
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                VStack(spacing: 8) {
                    ForEach(candidatos, id: \.address) { c in
                        Candidato(c: c, elegido: direccion == sinEsquema(c.address)) {
                            direccion = sinEsquema(c.address)
                            focoUsuario = true
                        }
                    }
                }
            }
        }
    }

    private func buscar() async {
        guard !buscando else { return }
        buscando = true
        defer { buscando = false }
        do {
            candidatos = try await link.discover()
            if candidatos.count == 1, direccion.isEmpty { direccion = sinEsquema(candidatos[0].address) }
            // Keyboard first: straight to what is still missing.
            if !focoDireccion && !focoUsuario && !focoContrasena {
                if direccion.isEmpty { focoDireccion = true } else { focoUsuario = true }
            }
        } catch {
            candidatos = []
            errorGeneral = Textos.falloBusqueda(error)
        }
    }

    private func entrar() {
        guard !entrando else { return }
        errorDireccion = nil; errorCredenciales = nil; errorGeneral = nil
        let dir = direccion.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !dir.isEmpty else {
            errorDireccion = "Elige un gateway o escribe su dirección."
            focoDireccion = true
            return
        }
        // Checked here too (the agent checks again): an empty field never reaches the gateway.
        // The password is never trimmed: spaces are part of it.
        guard !usuario.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !contrasena.isEmpty, contrasena.utf8.count <= 1024 else {
            errorCredenciales = Textos.fallo(.badCredentialsInput)
            focoContrasena = !usuario.isEmpty
            focoUsuario = usuario.isEmpty
            return
        }
        let (u, c) = (usuario, contrasena)
        contrasena = "" // whatever happens next, the field is empty again
        entrando = true
        Task {
            defer { entrando = false }
            do {
                switch try await link.login(address: dir, username: u, password: c) {
                case .loggedIn:
                    nav.seccion = .estado
                case .failed(let f):
                    switch f {
                    case .badAddress, .unreachable, .notAGateway, .needsSecureAddress, .insecureTransport:
                        errorDireccion = Textos.fallo(f); focoDireccion = true
                    case .invalidCredentials, .badCredentialsInput:
                        errorCredenciales = Textos.fallo(f); focoContrasena = true
                    default:
                        errorGeneral = Textos.fallo(f)
                    }
                }
            } catch {
                errorGeneral = Textos.fallo(error)
            }
        }
    }
}

private struct Candidato: View {
    let c: WireCandidate
    let elegido: Bool
    let elegir: () -> Void

    var body: some View {
        Button(action: elegir) {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(c.name).font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
                    Text(detalle).meta().lineLimit(1)
                }
                Spacer(minLength: 8)
                Image(systemName: elegido ? "checkmark.circle.fill" : "circle")
                    .font(.system(size: 16))
                    .foregroundStyle(elegido ? Color.hueso : Color.huesoTenue)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .frame(minHeight: 56)
            .background(RoundedRectangle(cornerRadius: Medida.radioNodo).fill(Color.piedra))
            .overlay {
                RoundedRectangle(cornerRadius: Medida.radioNodo)
                    .strokeBorder(elegido ? Color.hueso : Color.veta, lineWidth: elegido ? 1.5 : 1)
            }
            .contentShape(RoundedRectangle(cornerRadius: Medida.radioNodo))
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(c.name), \(detalle)")
        .accessibilityAddTraits(elegido ? .isSelected : [])
    }

    /// The local candidate is «en este Mac», never presented as verified (any local user could
    /// listen on 127.0.0.1:8004).
    private var detalle: String {
        let local = c.address.contains("127.0.0.1") || c.address.contains("localhost")
        return local ? "en este Mac · versión \(c.version)" : "\(sinEsquema(c.address)) · versión \(c.version)"
    }
}

private struct YaConSesion: View {
    let status: WireStatus
    @Environment(Navegacion.self) private var nav

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Ya has iniciado sesión en este Mac.").titularFogar()
                    .fixedSize(horizontal: false, vertical: true)
                Text(status.gateway.map { "\(Textos.cuenta($0)). Para entrar con otra cuenta o en otro gateway, cierra sesión primero en Ajustes." }
                     ?? "Para entrar con otra cuenta o en otro gateway, cierra sesión primero en Ajustes.")
                    .font(Tipo.cuerpo).suave()
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 12) {
                Button("Ver el estado") { nav.seccion = .estado }
                    .buttonStyle(Pildora(variante: .principal))
                Button("Ir a Ajustes") { nav.seccion = .ajustes }
                    .buttonStyle(Pildora())
            }
        }
    }
}

private struct SinAgente: View {
    let state: AgentState
    @Environment(AgentLink.self) private var link

    var body: some View {
        let t = Textos.titular(state)
        VStack(alignment: .leading, spacing: 24) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Primero, el agente.").titularFogar()
                Text("Para iniciar sesión hace falta el agente de Fogar en este Mac. \(t.sub)")
                    .font(Tipo.cuerpo).suave()
                    .fixedSize(horizontal: false, vertical: true)
                Estado(tono: t.tono, texto: t.titulo).font(Tipo.texto(14, peso: 550)).foregroundStyle(Color.hueso)
            }
            Button("Volver a llamar") { link.retryNow() }
                .buttonStyle(Pildora())
        }
    }
}

/// Addresses as the person writes them (the hint says «sin http://»).
func sinEsquema(_ address: String) -> String {
    address.replacingOccurrences(of: "http://", with: "")
}

/// macOS 26 draws a soft scroll-edge band under the title bar; on this full-bleed screen it reads
/// as a seam across the living background.
private struct SinBordeDeScroll: ViewModifier {
    func body(content: Content) -> some View {
        if #available(macOS 26, *) { content.scrollEdgeEffectHidden(true, for: .top) } else { content }
    }
}
