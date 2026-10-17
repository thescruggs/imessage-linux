# SPDX-License-Identifier: SSPL-1.0
# The fake daemon needs dbus-python and PyGObject; pick an interpreter that has them.
find_package(Python3 COMPONENTS Interpreter)
if(NOT KMESSAGES_TEST_PYTHON)
    foreach(py ${Python3_EXECUTABLE} /usr/bin/python3 python3)
        execute_process(COMMAND ${py} -c "import dbus, dbus.service, dbus.mainloop.glib; from gi.repository import GLib"
            RESULT_VARIABLE _py_ok OUTPUT_QUIET ERROR_QUIET)
        if(_py_ok EQUAL 0)
            set(KMESSAGES_TEST_PYTHON ${py} CACHE FILEPATH "Python with dbus-python + PyGObject for the fake daemon")
            break()
        endif()
    endforeach()
endif()
if(NOT KMESSAGES_TEST_PYTHON)
    message(FATAL_ERROR "Tests need a python3 with dbus-python and PyGObject (python-dbus, python-gobject); or configure with -DBUILD_TESTING=OFF")
endif()
message(STATUS "Fake daemon interpreter: ${KMESSAGES_TEST_PYTHON}")
find_program(DBUS_RUN_SESSION dbus-run-session REQUIRED)

set(KMESSAGES_SCREENSHOT_DIR ${CMAKE_BINARY_DIR}/screenshots)

add_library(kmessages_testutil STATIC fakedaemon.cpp fakedaemon.h)
target_compile_definitions(kmessages_testutil PUBLIC
    PYTHON3_EXECUTABLE="${KMESSAGES_TEST_PYTHON}"
    FAKE_DAEMON_PY="${CMAKE_CURRENT_SOURCE_DIR}/fake_daemon.py"
    SCREENSHOT_DIR="${KMESSAGES_SCREENSHOT_DIR}")
target_include_directories(kmessages_testutil PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(kmessages_testutil PUBLIC Qt6::Core Qt6::DBus Qt6::Test)
target_compile_options(kmessages_testutil PRIVATE -Wall -Wextra)

# Every test runs on its own private session bus (no service activation).
function(kmessages_add_test name)
    add_executable(${name} ${name}.cpp)
    target_compile_options(${name} PRIVATE -Wall -Wextra)
    target_link_libraries(${name} PRIVATE kmessages_core kmessages_testutil Qt6::Test ${ARGN})
    add_test(NAME ${name}
        COMMAND ${DBUS_RUN_SESSION} --config-file=${CMAKE_CURRENT_SOURCE_DIR}/session.conf -- $<TARGET_FILE:${name}>)
    set_tests_properties(${name} PROPERTIES
        TIMEOUT 180
        ENVIRONMENT "KMESSAGES_TEST_PRIVATE_BUS=1;QT_QPA_PLATFORM=offscreen;QT_QUICK_BACKEND=software;LC_ALL=C.UTF-8;LANGUAGE=C;TZ=UTC")
endfunction()

kmessages_add_test(tst_json)
kmessages_add_test(tst_models)
kmessages_add_test(tst_contacts)
kmessages_add_test(tst_screens Qt6::QuickControls2 KF6::IconThemes)
