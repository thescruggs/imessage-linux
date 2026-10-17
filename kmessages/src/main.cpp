// SPDX-License-Identifier: SSPL-1.0
#include "appcontroller.h"
#include "qmlsetup.h"

#include <KAboutData>
#include <KDBusService>
#include <KIconTheme>
#include <KLocalizedString>

#include <QApplication>
#include <QCommandLineParser>
#include <QDebug>
#include <QIcon>
#include <QImage>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QTimer>

#include <optional>

using namespace Qt::StringLiterals;

int main(int argc, char **argv)
{
    KIconTheme::initTheme();
    QApplication app(argc, argv);
    KLocalizedString::setApplicationDomain("kmessages");

    KAboutData about(u"kmessages"_s,
                     i18n("Messages"),
                     QStringLiteral(KMESSAGES_VERSION),
                     i18n("iMessage for KDE Plasma, backed by imessaged"),
                     KAboutLicense::Custom,
                     i18n("© imessage-linux contributors"));
    about.setLicenseText(i18n("Server Side Public License, version 1"));
    about.setDesktopFileName(u"org.imessage.kmessages"_s);
    KAboutData::setApplicationData(about);
    QApplication::setWindowIcon(QIcon::fromTheme(u"dialog-messages"_s));
    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        QQuickStyle::setStyle(u"org.kde.desktop"_s);
    }

    QCommandLineParser parser;
    const QCommandLineOption minimized(u"minimized"_s, i18n("Start hidden in the system tray."));
    const QCommandLineOption noTray(u"no-tray"_s, i18n("Do not show a system tray icon."));
    const QCommandLineOption chat(u"chat"_s, i18n("Open the conversation <id>."), u"id"_s);
    const QCommandLineOption screenshot(u"screenshot"_s,
                                        i18n("Developer: save a screenshot of the main window to <file> after it settled and exit."),
                                        u"file"_s);
    const QCommandLineOption screenshotDelay(u"screenshot-delay"_s, i18n("Developer: milliseconds to wait before --screenshot (default 4000)."), u"ms"_s);
    parser.addOptions({minimized, noTray, chat, screenshot, screenshotDelay});
    about.setupCommandLine(&parser);
    parser.process(app);
    about.processCommandLine(&parser);

    // One instance per session: a second launch raises the first one. A
    // --screenshot run is a throwaway instance: registering would forward it
    // to the running app (which ignores the option and raises its window).
    std::optional<KDBusService> service;
    if (!parser.isSet(screenshot)) {
        service.emplace(KDBusService::Unique);
    }

    km::AppOptions options;
    options.tray = !parser.isSet(noTray);
    // A screenshot run sits beside the real instance: no duplicate notifications.
    options.notifications = !parser.isSet(screenshot);
    km::AppController controller(QDBusConnection::sessionBus(), options);

    QQmlApplicationEngine engine;
    km::setupQmlEngine(engine, &controller);
    engine.load(km::mainQmlUrl());
    if (engine.rootObjects().isEmpty()) {
        return 1;
    }
    auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().first());
    controller.setWindow(window);

    if (service) {
        QObject::connect(&*service, &KDBusService::activateRequested, &controller, [&](const QStringList &args, const QString &) {
            QCommandLineParser p;
            p.addOptions({minimized, noTray, chat});
            p.parse(args);
            if (p.isSet(chat)) {
                controller.openChat(p.value(chat));
            } else {
                controller.showWindow();
            }
        });
    }

    const bool hidden = controller.trayAvailable() && (parser.isSet(minimized) || controller.settings()->startMinimized());
    if (!hidden) {
        controller.showWindow();
    }
    if (parser.isSet(chat)) {
        controller.openChat(parser.value(chat));
    }
    if (parser.isSet(screenshot)) {
        const QString path = parser.value(screenshot);
        const int delay = parser.isSet(screenshotDelay) ? parser.value(screenshotDelay).toInt() : 4000;
        QTimer::singleShot(delay, window, [window, path, &app] {
            const QImage img = window->grabWindow();
            const bool ok = !img.isNull() && img.save(path);
            qInfo().noquote() << (ok ? u"screenshot saved to"_s : u"screenshot FAILED for"_s) << path;
            app.exit(ok ? 0 : 1);
        });
    }
    return app.exec();
}
